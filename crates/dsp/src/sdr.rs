//! RTL-SDR receivers: channelizer, per-channel detectors, burst recorders, audio taps.
//!
//! The device threads (librtlsdr, the USB callback queue, reopen loop) live in the receiver
//! binary. It feeds blocks to `SdrSource::process_block` and keeps the stream fields
//! (`connected`, `streaming`, `last_block_rx`, `dev`, ...) up to date.

use num_complex::Complex32;
use railspotting_common::{label_has, nominal_noise, Clock, UHF_MHZ};
use rustfft::{Fft, FftPlanner};
use serde_json::{json, Map, Value};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use crate::bursts::{hanning, BurstRecorder};
use crate::voice::AudioTap;
use crate::{eot, round_even, round_even_int};

pub const FFT_SIZE: usize = 4096;
pub const HALF_FRACTION: f64 = 0.46; // usable fraction of the sample rate on each side
pub const DC_GUARD_MHZ: f64 = 0.020;
pub const CHANNEL_HALF_MHZ: f64 = 0.005;
pub const CENTER_MARGIN_MHZ: f64 = DC_GUARD_MHZ + CHANNEL_HALF_MHZ;
pub const CENTER_STEP_MHZ: f64 = 0.005;
pub const MAX_CLEARANCE_MHZ: f64 = 0.2;
pub const QUEUE_BLOCKS: usize = 50;
pub const STALL_S: f64 = 10.0;
pub const REOPEN_DELAY_S: f64 = 3.0;
pub const PREROLL_S: f64 = 0.3;
pub const BURST_PAD_S: f64 = 0.25;
pub const FLOOR_HISTORY_S: f64 = 30.0;
pub const FLOOR_MIN_WINDOWS: usize = 20;
pub const FLOOR_EVERY: usize = 25;
pub const FLOOR_PERCENTILE: f64 = 20.0;
pub const REF_NOISE_OFFSET_HZ: f64 = 50_000.0;
pub const CLIP_LEVEL: f32 = 0.98;
// A locked-up dongle (seen after a run of failed USB re-inits) streams random bytes: values spread
// evenly over the whole range whatever the gain, with no relation between one sample and the next.
// Both tests must hold. Flatness (emptiest / fullest of 8 histogram bins) is ~0.97 for random bytes
// and low for noise, but a strong carrier with some noise can reach 0.85; such a carrier is
// smooth from sample to sample (lag-1 correlation near 1), random bytes are not (~0.005).
pub const CORRUPT_FLATNESS: f64 = 0.8;
pub const CORRUPT_MAX_LAG1: f64 = 0.05;
pub const CORRUPT_BLOCKS: u32 = 10; // in a row (about 2 s) before the dongle is flagged
pub const CORRUPT_REOPEN_S: f64 = 60.0; // reopen a corrupt dongle at most this often
pub const TUNERS: [(i32, &str); 7] = [(0, "unknown"), (1, "E4000"), (2, "FC0012"), (3, "FC0013"),
                                      (4, "FC2580"), (5, "R820T/R820T2"), (6, "R828D")];

pub fn tuner_name(t: i32) -> &'static str {
    TUNERS.iter().find(|(k, _)| *k == t).map_or("unknown", |(_, n)| n)
}

pub fn max_event_s(kind: &str) -> f64 {
    if kind == "telemetry" { 3.0 } else { 120.0 }
}

pub fn role_default_center(role: &str) -> f64 {
    // ITC: between the national channel (102) and the common base channels, 101 to 125 in view
    match role { "vhf" => 161.5452, "eot" => 458.1385, "hot" => 453.1385, "itc" => 220.3, _ => 460.0 }
}

fn histogram_flatness(it: impl Iterator<Item = f32>) -> f64 {
    let mut counts = [0u64; 8];
    for x in it {
        let b = (((x + 1.0f32) * 4.0f32) as i32).clamp(0, 7);
        counts[b as usize] += 1;
    }
    let mn = *counts.iter().min().unwrap_or(&0);
    let mx = *counts.iter().max().unwrap_or(&0);
    mn as f64 / mx.max(1) as f64
}

/// Emptiest / fullest of 8 equal bins over [-1, 1].
pub fn sample_flatness(x: &[f32]) -> f64 {
    histogram_flatness(x.iter().copied())
}

/// |correlation| between each sample and the next: ~0 for random bytes, high for real signals.
pub fn lag1_correlation(x: &[Complex32]) -> f64 {
    if x.is_empty() {
        return 1.0;
    }
    let n = x.len() as f64;
    let (mut mr, mut mi) = (0.0f64, 0.0f64);
    for v in x {
        mr += v.re as f64;
        mi += v.im as f64;
    }
    let (mr, mi) = (mr / n, mi / n);
    let mut energy = 0.0f64;
    let (mut cr, mut ci) = (0.0f64, 0.0f64);
    let mut prev: Option<(f64, f64)> = None;
    for v in x {
        let (a, b) = (v.re as f64 - mr, v.im as f64 - mi);
        energy += a * a + b * b;
        if let Some((pa, pb)) = prev {
            // conj(prev) * cur
            cr += pa * a + pb * b;
            ci += pa * b - pb * a;
        }
        prev = Some((a, b));
    }
    if energy > 0.0 { (cr * cr + ci * ci).sqrt() / energy } else { 1.0 }
}

pub fn frames_per_window(rate: f64) -> usize {
    ((0.02 * rate / FFT_SIZE as f64).round_ties_even() as usize).max(4)
}

pub fn block_samples(rate: f64) -> usize {
    FFT_SIZE * frames_per_window(rate) * 2
}

pub fn half_span_mhz(rate: f64) -> f64 {
    rate * HALF_FRACTION / 1e6
}

// plan and coverage ------------------------------------------------------------------

/// A channel or reference frequency, numeric or a numeric string.
pub fn freq_of(c: &Value) -> f64 {
    match c.get("freq") {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(f64::NAN),
        _ => f64::NAN,
    }
}

/// (channels this role takes, reference or None).
pub fn wanted(role: &str, channels: &[Value], reference: Option<&Value>) -> (Vec<Value>, Option<Value>) {
    // ITC finds its own channels from base station beacons (itc_rx); the headend's plan is not
    // for it
    if role == "itc" {
        return (vec![], None);
    }
    if role == "vhf" {
        let chans = channels.iter().filter(|c| freq_of(c) < UHF_MHZ).cloned().collect();
        let r = reference.filter(|r| !r.is_null() && freq_of(r) < UHF_MHZ).cloned();
        return (chans, r);
    }
    if role == "eot" || role == "hot" {
        let chans = channels.iter()
            .filter(|c| c.get("kind").and_then(Value::as_str) == Some("telemetry") && label_has(c, role))
            .cloned().collect();
        return (chans, None);
    }
    (channels.iter().filter(|c| freq_of(c) >= UHF_MHZ).cloned().collect(), None)
}

/// Most clearance (up to 200 kHz) from every channel, then nearest the midpoint.
///
/// Works in integer Hz so the grid is exact. Returns None when the channels
/// don't fit in one window.
pub fn auto_center(freqs: &[f64], half: f64) -> Option<f64> {
    auto_center_with(freqs, half, CENTER_MARGIN_MHZ, CENTER_STEP_MHZ)
}

pub fn auto_center_with(freqs: &[f64], half: f64, margin: f64, step: f64) -> Option<f64> {
    if freqs.is_empty() {
        return None;
    }
    let f: Vec<i64> = freqs.iter().map(|x| (x * 1e6).round_ties_even() as i64).collect();
    let r = |x: f64| (x * 1e6).round_ties_even() as i64;
    let (h, m, s) = (r(half), r(margin), r(step));
    let (fmin, fmax) = (*f.iter().min()?, *f.iter().max()?);
    let (lo, hi) = (fmax - (h - m), fmin + (h - m));
    if hi < lo {
        return None;
    }
    let mid = (fmin + fmax) as f64 / 2.0;
    let cap = r(MAX_CLEARANCE_MHZ);
    let mut best: Option<(i64, f64)> = None;
    let mut c = lo;
    while c < hi + s {
        let d = f.iter().map(|x| (c - x).abs()).min().unwrap_or(0);
        if d >= m {
            let score = d.min(cap) as f64 / 1000.0 - (c as f64 - mid).abs() / 1e6;
            if best.is_none_or(|(_, b)| score > b) {
                best = Some((c, score));
            }
        }
        c += s;
    }
    best.map(|(c, _)| c as f64 / 1e6)
}

pub fn covers(center: f64, freq: f64, half: f64) -> bool {
    let d = (freq - center).abs();
    CENTER_MARGIN_MHZ - 1e-9 <= d && d <= half - CHANNEL_HALF_MHZ + 1e-9
}

/// The window one dongle covers: center, covered and uncovered channels, and the reference
/// when it fits. `center` and `window` are None before the first plan.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WindowPlan {
    pub center: Option<f64>,
    pub covered: Vec<Value>,
    pub uncovered: Vec<Value>,
    pub reference: Option<Value>,
    pub half: f64,
    pub window: Option<[f64; 2]>,
}

impl WindowPlan {
    pub fn to_json(&self) -> Value {
        json!({"center": self.center, "covered": self.covered, "uncovered": self.uncovered,
               "reference": self.reference, "half": self.half, "window": self.window})
    }
}

/// Center, covered and uncovered channels, and whether the reference fits.
pub fn plan_window(role: &str, rate: f64, channels: &[Value], reference: Option<&Value>,
                   fixed_center: Option<f64>, previous: Option<f64>) -> WindowPlan {
    let (chans, r) = wanted(role, channels, reference);
    let half = half_span_mhz(rate);
    let freqs: Vec<f64> = chans.iter().map(freq_of).collect();
    let center = match fixed_center.filter(|c| *c != 0.0) {
        Some(c) => c,
        None => {
            let mut center = None;
            if let Some(rv) = &r {
                let mut all = freqs.clone();
                all.push(freq_of(rv));
                center = auto_center(&all, half);
            }
            if center.is_none() {
                center = auto_center(&freqs, half);
            }
            if center.is_none() && !freqs.is_empty() {
                let mn = freqs.iter().cloned().fold(f64::INFINITY, f64::min);
                let mx = freqs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
                center = Some(round_even((mn + mx) / 2.0, 4));
            }
            if center.is_none() {
                if let Some(rv) = &r {
                    center = auto_center(&[freq_of(rv)], half);
                }
            }
            center.unwrap_or_else(|| previous.filter(|p| *p != 0.0).unwrap_or_else(|| role_default_center(role)))
        }
    };
    let center = round_even(center, 6);
    let mut covered = vec![];
    let mut uncovered = vec![];
    for c in chans {
        if covers(center, freq_of(&c), half) { covered.push(c) } else { uncovered.push(c) }
    }
    let ref_in = r.filter(|rv| covers(center, freq_of(rv), half));
    WindowPlan { center: Some(center), covered, uncovered, reference: ref_in, half,
                 window: Some([round_even(center - half, 4), round_even(center + half, 4)]) }
}

// detector ------------------------------------------------------------------------

/// The `q`th percentile with linear interpolation between the closest ranks.
pub fn percentile(values: &mut [f64], q: f64) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    values.sort_by(|a, b| a.total_cmp(b));
    let pos = q / 100.0 * (values.len() - 1) as f64;
    let i = pos.floor() as usize;
    let t = pos - i as f64;
    let a = values[i];
    let b = values[(i + 1).min(values.len() - 1)];
    let diff = b - a;
    // interpolate from the nearer end, so results stay monotonic
    if t >= 0.5 { b - diff * (1.0 - t) } else { a + diff * t }
}

/// One transmission on a channel as the detector tracks it.
#[derive(Debug, Clone, PartialEq)]
pub struct DetEvent {
    pub event_id: String,
    pub start_ts: f64,
    pub last_above: f64,
    pub peak_snr: f64,
    pub snr_sum: f64,
    pub samples: i64,
    pub floor: f64,
    pub truncated: bool,
    pub end_ts: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DetAction {
    /// The event is now `detector.event`.
    Open,
    Update,
    Closed(DetEvent),
}

/// Signal-above-floor detector for one channel, fed one dB value per window.
#[derive(Debug, Clone)]
pub struct ChannelDetector {
    pub freq: f64,
    pub ch: Value,
    pub kind: String,
    pub label: String,
    pub railroad: Value,
    pub proximity: Value,
    pub window_s: f64,
    pub margin_db: f64,
    pub min_windows: usize,
    pub nominal: i64,
    pub max_event_s: f64,
    pub close_after: f64,
    pub quiet: VecDeque<(f64, f64)>,
    pub recent: VecDeque<(f64, f64)>,
    pub floor: Option<f64>,
    pub quiet_count: usize,
    pub pending: Vec<(f64, f64)>,
    pub event: Option<DetEvent>,
    pub last_db: Option<f64>,
    scratch: Vec<f64>,
}

impl ChannelDetector {
    pub fn new(freq: f64, ch: Value, kind: &str, label: &str, window_s: f64) -> Self {
        Self::with(freq, ch, kind, label, window_s, 6.0, 2, Value::Null, Value::Null)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with(freq: f64, ch: Value, kind: &str, label: &str, window_s: f64, margin_db: f64,
                min_windows: usize, railroad: Value, proximity: Value) -> Self {
        ChannelDetector {
            freq, ch, kind: kind.into(), label: label.into(), railroad, proximity, window_s, margin_db,
            min_windows, nominal: nominal_noise(freq), max_event_s: max_event_s(kind),
            close_after: (3.0 * window_s).max(0.3), quiet: VecDeque::new(), recent: VecDeque::new(), floor: None,
            quiet_count: 0, pending: vec![], event: None, last_db: None, scratch: vec![],
        }
    }

    pub fn receiving(&self) -> bool {
        self.event.is_some()
    }

    fn percentile_of(&mut self, from_recent: bool) -> f64 {
        self.scratch.clear();
        let src = if from_recent { &self.recent } else { &self.quiet };
        self.scratch.extend(src.iter().map(|(_, d)| *d));
        percentile(&mut self.scratch, FLOOR_PERCENTILE)
    }

    fn add_quiet(&mut self, ts: f64, db: f64) {
        self.quiet.push_back((ts, db));
        while self.quiet.front().is_some_and(|q| ts - q.0 > FLOOR_HISTORY_S) {
            self.quiet.pop_front();
        }
        self.quiet_count += 1;
        if (self.floor.is_none() && self.quiet.len() >= FLOOR_MIN_WINDOWS)
            || (self.floor.is_some() && self.quiet_count >= FLOOR_EVERY) {
            self.floor = Some(self.percentile_of(false));
            self.quiet_count = 0;
        }
    }

    /// None or the action: open, update or closed.
    pub fn feed(&mut self, ts: f64, db: f64) -> Option<DetAction> {
        self.last_db = Some(db);
        self.recent.push_back((ts, db));
        while self.recent.front().is_some_and(|r| ts - r.0 > self.max_event_s) {
            self.recent.pop_front();
        }
        let Some(floor) = self.floor else {
            self.add_quiet(ts, db);
            return None;
        };
        let above = db >= floor + self.margin_db;
        let Some(ev) = self.event.as_mut() else {
            if !above {
                self.pending.clear();
                self.add_quiet(ts, db);
                return None;
            }
            self.pending.push((ts, db));
            if self.pending.len() < self.min_windows {
                return None;
            }
            let snrs: Vec<f64> = self.pending.iter().map(|(_, d)| d - floor).collect();
            self.event = Some(DetEvent {
                event_id: uuid::Uuid::new_v4().to_string(), start_ts: self.pending[0].0, last_above: ts,
                peak_snr: snrs.iter().cloned().fold(f64::NEG_INFINITY, f64::max), snr_sum: snrs.iter().sum(),
                samples: snrs.len() as i64, floor, truncated: false, end_ts: None,
            });
            self.pending.clear();
            return Some(DetAction::Open);
        };
        let mut result = None;
        if above {
            let snr = db - floor;
            ev.last_above = ts;
            ev.peak_snr = ev.peak_snr.max(snr);
            ev.snr_sum += snr;
            ev.samples += 1;
            result = Some(DetAction::Update);
        } else if ts - ev.last_above >= self.close_after {
            return Some(DetAction::Closed(self.finish()));
        }
        if ts - ev.start_ts > self.max_event_s {
            ev.truncated = true;
            ev.last_above = ts;
            let new = self.percentile_of(true);
            self.floor = Some(new);
            self.quiet = self.recent.clone();
            self.quiet_count = 0;
            log::warn!("{:.4} MHz: noise rose {:+.1} dB; floor relearned", self.freq, new - floor);
            return Some(DetAction::Closed(self.finish()));
        }
        result
    }

    pub fn force_close(&mut self) -> Option<DetEvent> {
        self.event.as_ref()?;
        Some(self.finish())
    }

    fn finish(&mut self) -> DetEvent {
        let mut ev = self.event.take().expect("open event");
        self.pending.clear();
        ev.end_ts = Some(ev.last_above + self.window_s);
        ev
    }

    pub fn payload(&self, ev: &DetEvent, state: &str) -> Map<String, Value> {
        let avg = ev.snr_sum / ev.samples.max(1) as f64;
        let closed = state == "closed";
        let mut out = Map::new();
        out.insert("event_id".into(), json!(ev.event_id));
        out.insert("state".into(), json!(state));
        out.insert("start_ts".into(), json!(round_even(ev.start_ts, 2)));
        out.insert("end_ts".into(), match (closed, ev.end_ts) {
            (true, Some(e)) => json!(round_even(e, 2)),
            _ => Value::Null,
        });
        out.insert("freq".into(), json!(round_even(self.freq, 4)));
        out.insert("ch".into(), self.ch.clone());
        out.insert("kind".into(), json!(self.kind));
        out.insert("peak_rssi".into(), json!(round_even_int(self.nominal as f64 + ev.peak_snr)));
        out.insert("avg_rssi".into(), json!(round_even_int(self.nominal as f64 + avg)));
        out.insert("samples".into(), json!(ev.samples));
        if closed && ev.truncated {
            out.insert("truncated".into(), json!(true));
        }
        out
    }
}

// source ---------------------------------------------------------------------------

/// Where events go (the agent's emit).
pub type Emit = Arc<dyn Fn(Map<String, Value>) + Send + Sync>;

/// Called exactly once with the transcript (or None).
pub type TranscriptDone = Box<dyn FnOnce(Option<Map<String, Value>>) + Send>;

/// The speech side, implemented by the receiver. `submit` must never block for long and must
/// call `done` exactly once, possibly from inside `submit` (queue full).
pub trait Transcriber: Send + Sync {
    fn submit(&self, audio: Vec<f32>, done: TranscriptDone, tag: &str, event: Option<Value>);
    /// Whether calls other than detectors are transcribed (`transcribe_other_calls`).
    fn other_calls(&self) -> bool {
        true
    }
}

/// What the processing side needs from an open dongle.
pub trait DeviceControl: Send + Sync {
    fn cancel_read_async(&self);
    fn set_center(&self, center_hz: f64);
}

type RefDone = Box<dyn FnOnce(Vec<f32>) + Send>;

struct ExtraTap {
    tap: AudioTap,
    seconds: f64,
    done: Option<RefDone>,
}

/// Inclusive FFT bin range of a channel in the shifted spectrum (empty when lo > hi).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bins {
    pub lo: usize,
    pub hi: usize,
    pub empty: bool,
}

impl Bins {
    fn sum(&self, row: &[f32]) -> f32 {
        if self.empty { 0.0 } else { row[self.lo..=self.hi].iter().sum() }
    }
}

/// One dongle's processing: windows, detectors, reference, recorders and audio taps.
pub struct SdrSource {
    pub serial: String,
    pub role: String,
    pub name: String,
    pub antenna: String,
    pub gain: Value,
    pub rate: f64,
    pub fixed_center: Option<f64>,
    pub margin_db: f64,
    pub min_windows: usize,
    pub record: bool,
    pub record_keep: usize,
    pub state_dir: PathBuf,
    pub emit: Emit,
    pub transcriber: Option<Arc<dyn Transcriber>>,
    pub clock: Clock,

    pub fpw: usize,
    pub block: usize,
    pub window_s: f64,
    hann: Vec<f32>,
    fft: Arc<dyn Fft<f32>>,
    fft_buf: Vec<Complex32>,
    fft_scratch: Vec<Complex32>,
    power: Vec<f32>,

    pub detectors: Vec<ChannelDetector>,
    pub bins: Vec<Bins>, // parallel to detectors
    pub recorders: Vec<(f64, BurstRecorder)>,
    pub taps: Vec<(f64, AudioTap)>,
    extra_taps: Vec<ExtraTap>,
    preroll: VecDeque<Vec<Complex32>>,
    spare: Vec<Vec<Complex32>>,
    held: Arc<AtomicI64>,
    pub center: Option<f64>,
    pub plan: WindowPlan,
    pub ref_bins: Option<(Bins, Bins, Value)>,
    pub reference: Option<Value>,
    pub retune_to: Option<f64>,
    pub discard_until: f64,

    pub dev: Option<Arc<dyn DeviceControl>>,
    pub info: Map<String, Value>,
    pub plugged_in: Option<bool>, // from USB (sysfs); None when unknown
    pub usb_port: Option<String>,
    pub since: f64, // when connected or plugged_in last changed
    pub connected: bool,
    pub streaming: bool,
    pub last_block_rx: f64,
    pub last_stall_action: f64,
    pub stall_cancelled: bool,
    pub blocks: u64,
    pub overruns: u64,
    pub short_reads: u64,
    pub stalls: u64,
    pub clip_fraction: f64,
    pub corrupt: bool,
    pub corrupt_run: u32,
    pub last_corrupt_reopen: f64,
    pub last_eot: Option<Value>,
    last_transcript: Arc<Mutex<Option<Value>>>,
    pub last_error: Option<String>,
    /// (start, MHz) of calls opened in the last minute, to tell a call on one channel from a burst
    /// of interference that opens many channels at once
    recent_opens: std::collections::VecDeque<(f64, f64)>,
    /// An ITC dongle's decoder (role `itc`); its samples go there instead of to the detectors.
    pub itc: Option<crate::itc_rx::ItcRx>,
}

/// Other channels opening within this many seconds of a call make it part of a burst.
pub const SIMULTANEOUS_S: f64 = 1.0;

/// A numeric setting: a number, or a string that parses as one.
fn setting_f64(s: &Value, k: &str, d: f64) -> f64 {
    match s.get(k) {
        Some(Value::String(x)) => x.trim().parse().unwrap_or(d),
        Some(v) => v.as_f64().unwrap_or(d),
        None => d,
    }
}

/// Whether a JSON value counts as set: false for null, false, zero and empty strings, arrays
/// and objects.
pub(crate) fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|x| x != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// A value as log text: strings bare, null as `None`, booleans as `True` and `False`.
fn value_str(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "None".into(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => if *b { "True" } else { "False" }.into(),
        Some(v) => v.to_string(),
    }
}

impl SdrSource {
    /// `dev_cfg` is one `sdr.devices` entry, `sdr_settings` the `sdr` settings object.
    pub fn new(dev_cfg: &Value, sdr_settings: &Value, state_dir: &Path, emit: Emit,
               transcriber: Option<Arc<dyn Transcriber>>, clock: Clock) -> Self {
        let s = |k: &str| dev_cfg.get(k).filter(|v| !v.is_null());
        let serial = match s("serial") {
            Some(Value::String(x)) => x.clone(),
            Some(v) => v.to_string(),
            None => String::new(),
        };
        let role = s("role").and_then(Value::as_str).unwrap_or("vhf").to_string();
        let name = s("name").and_then(Value::as_str).filter(|x| !x.is_empty()).map(String::from)
            .unwrap_or_else(|| format!("{} SDR", role.to_uppercase()));
        let antenna = s("antenna").and_then(Value::as_str).filter(|x| !x.is_empty()).map(String::from)
            .unwrap_or_else(|| if role == "vhf" { "the VHF antenna" } else { "the UHF antenna" }.into());
        let gain = dev_cfg.get("gain").cloned().unwrap_or(json!(20.7));
        let rate = setting_f64(dev_cfg, "sample_rate", 2_400_000.0);
        let fixed_center = match dev_cfg.get("center_mhz") {
            None | Some(Value::Null) => None,
            Some(Value::String(x)) if x == "auto" => None,
            Some(Value::String(x)) => x.parse().ok(),
            Some(v) => v.as_f64(),
        };
        let fpw = frames_per_window(rate);
        let role_is_itc = role == "itc";
        let fft = FftPlanner::<f32>::new().plan_fft_forward(FFT_SIZE);
        let scratch_len = fft.get_inplace_scratch_len();
        let now = clock();
        SdrSource {
            serial,
            transcriber: if role == "vhf" { transcriber } else { None },
            role, name, antenna, gain, rate, fixed_center,
            margin_db: setting_f64(sdr_settings, "margin_db", 6.0),
            min_windows: setting_f64(sdr_settings, "min_windows", 2.0) as usize,
            record: sdr_settings.get("record_bursts").is_none_or(|v| truthy(Some(v))),
            record_keep: setting_f64(sdr_settings, "record_keep", 500.0) as usize,
            state_dir: state_dir.to_path_buf(),
            emit, clock,
            fpw,
            block: block_samples(rate),
            window_s: (fpw * FFT_SIZE) as f64 / rate,
            hann: hanning(FFT_SIZE).into_iter().map(|v| v as f32).collect(),
            fft, fft_buf: vec![], fft_scratch: vec![Complex32::new(0.0, 0.0); scratch_len], power: vec![],
            detectors: vec![], bins: vec![], recorders: vec![], taps: vec![], extra_taps: vec![],
            preroll: VecDeque::new(), spare: vec![], held: Arc::new(AtomicI64::new(0)),
            center: None, plan: WindowPlan::default(), ref_bins: None, reference: None, retune_to: None,
            discard_until: 0.0,
            dev: None,
            info: json!({"vendor": null, "product": null, "chipset": "RTL2832U", "tuner": null})
                .as_object().cloned().unwrap_or_default(),
            plugged_in: None, usb_port: None, since: now, connected: false, streaming: false,
            last_block_rx: 0.0, last_stall_action: 0.0, stall_cancelled: false,
            blocks: 0, overruns: 0, short_reads: 0, stalls: 0, clip_fraction: 0.0,
            corrupt: false, corrupt_run: 0, last_corrupt_reopen: 0.0,
            last_eot: None, last_transcript: Arc::new(Mutex::new(None)), last_error: None,
            recent_opens: std::collections::VecDeque::new(),
            itc: (role_is_itc).then(|| crate::itc_rx::ItcRx::new(dev_cfg, state_dir)),
        }
    }

    /// Voice events waiting for their transcript.
    pub fn held(&self) -> i64 {
        self.held.load(Ordering::SeqCst)
    }

    pub fn last_transcript(&self) -> Option<Value> {
        self.last_transcript.lock().map(|g| g.clone()).unwrap_or(None)
    }

    pub fn detector(&self, freq: f64) -> Option<&ChannelDetector> {
        self.detectors.iter().find(|d| (d.freq - freq).abs() < 1e-9)
    }

    // plan ---------------------------------------------------------------------------

    /// Recompute window and detectors. Runs at start and on every new config.
    pub fn set_plan(&mut self, channels: &[Value], reference: Option<&Value>) -> WindowPlan {
        let plan = plan_window(&self.role, self.rate, channels, reference, self.fixed_center, self.center);
        let new_center = plan.center.unwrap_or(0.0);
        let center_moved = self.center.is_some_and(|c| (new_center - c).abs() > 1e-6);
        let mut old: Vec<Option<ChannelDetector>> = std::mem::take(&mut self.detectors).into_iter().map(Some).collect();
        // Ordered map of frequency to an old detector (by index) or a new one. A repeated
        // frequency replaces the earlier entry in its place.
        enum Kept {
            Old(usize),
            New(Box<ChannelDetector>),
        }
        let mut keep: Vec<(f64, Kept)> = vec![];
        for c in &plan.covered {
            let f = round_even(freq_of(c), 4);
            let kind = c.get("kind").and_then(Value::as_str).unwrap_or("voice");
            let label = c.get("label").and_then(Value::as_str).unwrap_or("");
            let ch = c.get("ch").cloned().unwrap_or(Value::Null);
            let railroad = c.get("railroad").cloned().unwrap_or(Value::Null);
            let proximity = c.get("proximity").cloned().unwrap_or(Value::Null);
            let reuse = old.iter().position(|o| o.as_ref().is_some_and(|d| d.freq == f));
            let entry = match reuse {
                Some(i) if !center_moved && old[i].as_ref().is_some_and(|d| d.kind == kind) => {
                    if let Some(d) = old[i].as_mut() {
                        d.ch = ch;
                        d.label = label.into();
                        d.railroad = railroad;
                        d.proximity = proximity;
                    }
                    Kept::Old(i)
                }
                _ => Kept::New(Box::new(ChannelDetector::with(f, ch, kind, label, self.window_s, self.margin_db,
                                                              self.min_windows, railroad, proximity))),
            };
            match keep.iter().position(|(kf, _)| *kf == f) {
                Some(i) => keep[i].1 = entry,
                None => keep.push((f, entry)),
            }
        }
        let mut detectors = Vec::with_capacity(keep.len());
        for (_, k) in keep {
            match k {
                Kept::Old(i) => detectors.extend(old[i].take()),
                Kept::New(d) => detectors.push(*d),
            }
        }
        for mut det in old.into_iter().flatten() {
            self.close_detector(&mut det);
        }
        self.detectors = detectors;
        self.center = Some(new_center);
        self.plan = plan.clone();
        self.rebuild_bins();
        let mut old_recs = std::mem::take(&mut self.recorders);
        let mut recorders = vec![];
        for det in &self.detectors {
            if det.kind != "telemetry" {
                continue;
            }
            let f = det.freq;
            match old_recs.iter().position(|(rf, _)| *rf == f).filter(|_| !center_moved) {
                Some(i) => {
                    let (_, mut r) = old_recs.remove(i);
                    r.label = det.label.clone();
                    r.ch = det.ch.clone();
                    recorders.push((f, r));
                }
                None => recorders.push((f, BurstRecorder::new(
                    self.rate, (f - new_center) * 1e6, f, &det.label, det.ch.clone(),
                    Some(&self.state_dir.join("bursts")), self.record_keep, 4.0, Some(new_center), self.record))),
            }
        }
        self.recorders = recorders;
        if center_moved {
            self.taps.clear();
            self.extra_taps.clear();
            self.clear_preroll();
            if self.streaming {
                self.retune_to = Some(new_center);
            }
            log::info!("{}: retune to {:.4} MHz", self.name, new_center);
        }
        if let Some([lo, hi]) = plan.window {
            for c in &plan.uncovered {
                let label = value_str(c.get("label"));
                log::warn!("{}: {:.4} MHz ({}) is outside the window {:.4}-{:.4} MHz", self.name, freq_of(c),
                           label, lo, hi);
            }
        }
        plan
    }

    fn bins_at(&self, offset_hz: f64) -> Bins {
        let bw = self.rate / FFT_SIZE as f64;
        let k0 = (FFT_SIZE / 2) as f64 + offset_hz / bw;
        let lo = (k0 - CHANNEL_HALF_MHZ * 1e6 / bw).ceil() as i64;
        let hi = (k0 + CHANNEL_HALF_MHZ * 1e6 / bw).floor() as i64;
        let (lo, hi) = (lo.max(0), hi.min(FFT_SIZE as i64 - 1));
        Bins { lo: lo.max(0) as usize, hi: hi.max(0) as usize, empty: lo > hi }
    }

    fn rebuild_bins(&mut self) {
        let center = self.center.unwrap_or(0.0);
        self.bins = self.detectors.iter().map(|d| self.bins_at((d.freq - center) * 1e6)).collect();
        self.ref_bins = None;
        if let Some(r) = self.plan.reference.clone() {
            let off = (freq_of(&r) - center) * 1e6;
            let half_hz = self.plan.half * 1e6 - CHANNEL_HALF_MHZ * 1e6;
            let mut noise_off = off + REF_NOISE_OFFSET_HZ;
            if noise_off.abs() > half_hz || noise_off.abs() < CENTER_MARGIN_MHZ * 1e6 {
                noise_off = off - REF_NOISE_OFFSET_HZ;
            }
            self.ref_bins = Some((self.bins_at(off), self.bins_at(noise_off), r));
        }
    }

    pub fn capability(&self) -> Map<String, Value> {
        let p = &self.plan;
        let i = |k: &str| self.info.get(k).cloned().unwrap_or(Value::Null);
        let chs = |v: &Vec<Value>| Value::Array(v.iter().map(|c| c.get("ch").cloned().unwrap_or(Value::Null)).collect());
        let v = json!({
            "type": "sdr", "vendor": i("vendor"), "product": i("product"),
            "chipset": i("chipset"), "tuner": i("tuner"),
            "connected": self.connected && !self.corrupt, "role": self.role,
            "window_mhz": p.window,
            "channels": chs(&p.covered),
            "uncovered_channels": chs(&p.uncovered),
            "reference_in_window": p.reference.is_some(),
            "simultaneous": true, "squelch_free": true, "audio": true,
        });
        v.as_object().cloned().unwrap_or_default()
    }

    pub fn covers_kind(&self, kind: &str, label_word: Option<&str>) -> bool {
        if !self.connected {
            return false;
        }
        self.plan.covered.iter().any(|c| c.get("kind").and_then(Value::as_str) == Some(kind)
            && label_word.is_none_or(|w| label_has(c, w)))
    }

    // streaming ------------------------------------------------------------------------

    /// USB presence from the agent's check every 2 s. Unplugging closes the stream at once.
    pub fn set_plugged_in(&mut self, present: bool, port: Option<&str>) {
        if present {
            self.usb_port = port.map(String::from);
        }
        if Some(present) == self.plugged_in {
            return;
        }
        let first = self.plugged_in.is_none();
        self.plugged_in = Some(present);
        if first && present {
            return;
        }
        self.since = (self.clock)();
        if present {
            log::info!("{}: dongle {} plugged in (USB port {})", self.name, self.serial, port.unwrap_or("None"));
            return;
        }
        let from = self.usb_port.as_ref().map(|p| format!(" from USB port {p}")).unwrap_or_default();
        log::warn!("{}: dongle {} unplugged{}", self.name, self.serial, from);
        if let Some(dev) = &self.dev {
            if self.streaming {
                dev.cancel_read_async();
            }
        }
    }

    /// Device thread, before opening: the center to tune to (a pending retune is now moot).
    pub fn open_center(&mut self) -> f64 {
        self.retune_to = None;
        self.center.unwrap_or_else(|| role_default_center(&self.role))
    }

    /// Device thread, after the dongle is open and configured, just before streaming starts.
    pub fn stream_started(&mut self, dev: Arc<dyn DeviceControl>, info: Option<Map<String, Value>>) {
        if let Some(info) = info.filter(|i| !i.is_empty()) {
            self.info = info;
        }
        self.dev = Some(dev);
        self.connected = true;
        self.since = (self.clock)();
        self.last_error = None;
        let i = |k: &str| value_str(self.info.get(k));
        log::info!("{} connected: {} {}, tuner {}, {:.4} MHz, {:.2} MS/s, gain {} dB", self.name, i("vendor"),
                   i("product"), i("tuner"), self.center.unwrap_or(0.0), self.rate / 1e6, value_str(Some(&self.gain)));
        self.last_block_rx = (self.clock)();
        self.stall_cancelled = false;
        self.streaming = true;
    }

    /// Why a stream that returned without being stopped ended.
    pub fn stream_end_reason(&self) -> &'static str {
        if self.stall_cancelled { "stream stalled" } else { "stream ended" }
    }

    /// Device thread, when the stream ended or opening failed. `error` is None when stopping.
    /// Closes open events if the dongle was connected.
    pub fn stream_ended(&mut self, error: Option<&str>) {
        if let Some(msg) = error {
            if self.connected || self.last_error.as_deref() != Some(msg) {
                log::warn!("{} lost: {}", self.name, msg);
            }
            self.last_error = Some(msg.to_string());
        }
        let was = self.connected;
        self.streaming = false;
        self.connected = false;
        self.dev = None;
        if was {
            self.since = (self.clock)();
            self.close_all();
        }
    }

    /// Cancel the stream when no block has arrived for 10 s (at most once per 10 s).
    pub fn check_stall(&mut self, now: f64) -> bool {
        if !self.streaming || self.dev.is_none() {
            return false;
        }
        let idle = now - self.last_block_rx;
        if idle < STALL_S || now - self.last_stall_action < STALL_S {
            return false;
        }
        self.last_stall_action = now;
        self.stalls += 1;
        self.stall_cancelled = true;
        log::warn!("{}: no samples for {} s; reopening the dongle", self.name, idle as i64);
        if let Some(dev) = &self.dev {
            dev.cancel_read_async();
        }
        true
    }

    // processing -----------------------------------------------------------------------

    fn clear_preroll(&mut self) {
        while let Some(b) = self.preroll.pop_front() {
            self.spare.push(b);
        }
        self.spare.truncate(4);
    }

    /// Close every open event (the stream ended) and drop what was buffered.
    pub fn close_all(&mut self) {
        let mut dets = std::mem::take(&mut self.detectors);
        for det in &mut dets {
            self.close_detector(det);
        }
        self.detectors = dets;
        self.taps.clear();
        self.clear_preroll();
        for (_, rec) in &mut self.recorders {
            rec.reset();
        }
    }

    /// True while the dongle streams random bytes instead of samples (see CORRUPT_FLATNESS).
    fn check_corrupt(&mut self, used: &[Complex32]) -> bool {
        let fr = histogram_flatness(used.iter().step_by(4).map(|c| c.re));
        let fi = histogram_flatness(used.iter().step_by(4).map(|c| c.im));
        let flat = fr.min(fi);
        if flat < CORRUPT_FLATNESS || lag1_correlation(&used[..used.len().min(4096)]) > CORRUPT_MAX_LAG1 {
            self.corrupt_run = 0;
            if self.corrupt {
                self.corrupt = false;
                self.since = (self.clock)();
                log::info!("{}: dongle {} is sending normal samples again", self.name, self.serial);
            }
            return false;
        }
        self.corrupt_run += 1;
        if self.corrupt_run < CORRUPT_BLOCKS {
            return true; // skipped, but not flagged until it keeps up for ~2 s
        }
        let now = (self.clock)();
        if !self.corrupt {
            self.corrupt = true;
            self.since = now;
            let mut dets = std::mem::take(&mut self.detectors);
            for det in &mut dets {
                self.close_detector(det);
            }
            self.detectors = dets;
            log::warn!("{}: dongle {} is sending corrupt data (random bytes, not radio samples); unplug it \
                        for 10 s and plug it back in", self.name, self.serial);
        }
        if now - self.last_corrupt_reopen >= CORRUPT_REOPEN_S {
            if let Some(dev) = &self.dev {
                self.last_corrupt_reopen = now;
                log::info!("{}: reopening dongle {} to try to clear it", self.name, self.serial);
                dev.cancel_read_async();
            }
        }
        true
    }

    fn close_detector(&mut self, det: &mut ChannelDetector) {
        let r = det.force_close();
        self.taps.retain(|(f, _)| *f != det.freq);
        if let Some(ev) = r {
            (self.emit)(det.payload(&ev, "closed"));
        }
    }

    /// One block of samples; `ts` is the time of its end.
    pub fn process_block(&mut self, samples: &[Complex32], ts: f64) {
        if let Some(target) = self.retune_to.take() {
            if let Some(dev) = &self.dev {
                dev.set_center(target * 1e6);
            }
            self.discard_until = (self.clock)() + 0.05;
            return;
        }
        if ts < self.discard_until {
            return;
        }
        self.blocks += 1;
        let win_len = self.fpw * FFT_SIZE;
        let n_win = samples.len() / win_len;
        if samples.len() < self.block {
            self.short_reads += 1;
        }
        if n_win == 0 {
            return;
        }
        let used = &samples[..n_win * win_len];
        let clipped = used.iter().filter(|c| c.re.abs() > CLIP_LEVEL || c.im.abs() > CLIP_LEVEL).count();
        self.clip_fraction = clipped as f64 / used.len() as f64;
        if self.check_corrupt(used) {
            return; // random bytes: no events, and keep the learned floors
        }
        if let Some(itc) = self.itc.as_mut() {
            let center = self.center.or(self.plan.center).unwrap_or_else(|| role_default_center("itc"));
            itc.process(samples, ts - samples.len() as f64 / self.rate, center, self.rate);
            return;
        }
        self.spectrum(used, n_win);
        let t0 = ts - samples.len() as f64 / self.rate;

        let mut buf = self.spare.pop().unwrap_or_default();
        buf.clear();
        buf.extend_from_slice(samples);
        self.preroll.push_back(buf);
        let limit = PREROLL_S + self.block as f64 / self.rate;
        while self.preroll.iter().map(Vec::len).sum::<usize>() as f64 / self.rate > limit {
            if let Some(b) = self.preroll.pop_front() {
                self.spare.push(b);
            }
        }
        for (_, rec) in &mut self.recorders {
            rec.feed(samples, ts);
        }
        for (_, tap) in &mut self.taps {
            tap.feed(samples);
        }
        self.feed_extra_taps(samples);
        if let Some((sig_b, noise_b, r)) = &self.ref_bins {
            let ratios: Vec<f64> = (0..n_win).map(|w| {
                let row = &self.power[w * FFT_SIZE..(w + 1) * FFT_SIZE];
                let sig = (sig_b.sum(row) as f64).max(1e-20);
                let noise = (noise_b.sum(row) as f64).max(1e-20);
                10.0 * (sig / noise).log10()
            }).collect();
            let snr = eot::median(&ratios);
            self.reference = Some(json!({"name": r.get("name").cloned().unwrap_or(Value::Null),
                                         "freq": freq_of(r), "snr_db": round_even(snr, 1), "ts": ts}));
        }
        let mut dets = std::mem::take(&mut self.detectors);
        for (k, det) in dets.iter_mut().enumerate() {
            let b = self.bins[k];
            for i in 0..n_win {
                let row = &self.power[i * FFT_SIZE..(i + 1) * FFT_SIZE];
                let db = 10.0 * (b.sum(row).max(1e-20) as f64).log10();
                match det.feed(t0 + i as f64 * self.window_s, db) {
                    None | Some(DetAction::Update) => {}
                    Some(DetAction::Open) => self.on_open(det),
                    Some(DetAction::Closed(ev)) => self.on_closed(det, ev),
                }
            }
        }
        self.detectors = dets;
    }

    /// Mean power per window over its frames, fftshifted, into `self.power`.
    fn spectrum(&mut self, used: &[Complex32], n_win: usize) {
        self.fft_buf.clear();
        for frame in used.chunks_exact(FFT_SIZE) {
            self.fft_buf.extend(frame.iter().zip(&self.hann).map(|(s, w)| s * *w));
        }
        self.fft.process_with_scratch(&mut self.fft_buf, &mut self.fft_scratch);
        self.power.clear();
        self.power.resize(n_win * FFT_SIZE, 0.0);
        let half = FFT_SIZE / 2;
        for w in 0..n_win {
            let row = &mut self.power[w * FFT_SIZE..(w + 1) * FFT_SIZE];
            for f in 0..self.fpw {
                let spec = &self.fft_buf[(w * self.fpw + f) * FFT_SIZE..(w * self.fpw + f + 1) * FFT_SIZE];
                for (k, s) in spec.iter().enumerate() {
                    row[(k + half) % FFT_SIZE] += s.re * s.re + s.im * s.im;
                }
            }
            let n = self.fpw as f32;
            for v in row.iter_mut() {
                *v /= n;
            }
        }
    }

    fn on_open(&mut self, det: &ChannelDetector) {
        if det.kind == "voice" && self.transcriber.is_some() {
            let mut tap = AudioTap::new(self.rate, (det.freq - self.center.unwrap_or(0.0)) * 1e6);
            for b in &self.preroll {
                tap.feed(b);
            }
            self.taps.retain(|(f, _)| *f != det.freq);
            self.taps.push((det.freq, tap));
        }
        let ev = det.event.as_ref().expect("opened");
        self.recent_opens.retain(|(t, _)| ev.start_ts - t < 60.0);
        self.recent_opens.push_back((ev.start_ts, det.freq));
        (self.emit)(det.payload(ev, "open"));
    }

    fn on_closed(&mut self, det: &ChannelDetector, ev: DetEvent) {
        let mut payload = det.payload(&ev, "closed");
        let f = |k: &str| payload.get(k).and_then(Value::as_f64).unwrap_or(0.0);
        let dur = f("end_ts") - f("start_ts");
        log::info!("{}: {:.4} MHz {}, {:.1} s, peak {:+.0} dB SNR{}", self.name, det.freq, det.label, dur,
                   ev.peak_snr, if ev.truncated { " (truncated)" } else { "" });
        let tap = self.taps.iter().position(|(f, _)| *f == det.freq).map(|i| self.taps.remove(i).1);
        if det.kind == "telemetry" {
            if !ev.truncated {
                self.telemetry_closed(det, &ev, &mut payload);
            }
            (self.emit)(payload);
            return;
        }
        if let (Some(tap), false, Some(tr)) = (&tap, ev.truncated, &self.transcriber) {
            if tap.duration_s() >= 1.0 {
                self.held.fetch_add(1, Ordering::SeqCst);
                let (name, freq, label) = (self.name.clone(), det.freq, det.label.clone());
                let (held, last, emit) = (self.held.clone(), self.last_transcript.clone(), self.emit.clone());
                // other channels that opened with this one: a detector transmits on one channel,
                // interference opens many together
                let start = ev.start_ts;
                let mut others: Vec<f64> = self.recent_opens.iter()
                    .filter(|(t, f)| (t - start).abs() <= SIMULTANEOUS_S && (f - det.freq).abs() > 0.0005)
                    .map(|(_, f)| *f).collect();
                others.sort_by(f64::total_cmp);
                others.dedup();
                let event = json!({"event_id": payload["event_id"], "start_ts": payload["start_ts"],
                                   "end_ts": payload["end_ts"], "freq": payload["freq"],
                                   "railroad": det.railroad.clone(), "simultaneous": others.len()});
                let mut payload = payload;
                let done: TranscriptDone = Box::new(move |result| {
                    held.fetch_sub(1, Ordering::SeqCst);
                    if let Some(mut result) = result.filter(|r| !r.is_empty()) {
                        let detector = result.remove("_detector");
                        let mut lt = Map::new();
                        lt.insert("ts".into(), payload.get("end_ts").cloned().unwrap_or(Value::Null));
                        lt.insert("freq".into(), json!(freq));
                        lt.insert("label".into(), json!(label));
                        for (k, v) in &result {
                            lt.insert(k.clone(), v.clone());
                        }
                        let text = result.get("text").and_then(Value::as_str).unwrap_or("").to_string();
                        let conf = result.get("confidence").and_then(Value::as_f64).unwrap_or(0.0);
                        payload.insert("transcript".into(), Value::Object(result));
                        if let Some(d) = detector.filter(|d| truthy(Some(d))) {
                            payload.insert("detector".into(), d); // API contract v1.5
                        }
                        if let Ok(mut g) = last.lock() {
                            *g = Some(Value::Object(lt));
                        }
                        log::info!("{}: {:.4} MHz {} said: \"{}\" ({}%)", name, freq, label, text,
                                   round_even_int(conf * 100.0));
                    }
                    emit(payload);
                });
                tr.submit(tap.audio(), done, "voice", Some(event));
                return;
            }
        }
        (self.emit)(payload);
    }

    fn telemetry_closed(&mut self, det: &ChannelDetector, ev: &DetEvent, payload: &mut Map<String, Value>) {
        let Some((_, rec)) = self.recorders.iter().find(|(f, _)| *f == det.freq) else { return };
        let end = ev.end_ts.unwrap_or(ev.last_above);
        let (samples, t_first) = rec.extract(ev.start_ts - BURST_PAD_S, end + BURST_PAD_S);
        let mut packets = vec![];
        if label_has(&json!({"label": det.label}), "EOT") && !samples.is_empty() {
            packets = eot::decode32(&samples, rec.out_rate);
            if let Some(tel) = eot::telemetry_from(&packets) {
                log::info!("{}: {}", self.name, eot::describe(&tel));
                let mut le = Map::new();
                le.insert("ts".into(), payload.get("end_ts").cloned().unwrap_or(Value::Null));
                le.extend(tel.clone());
                payload.insert("telemetry".into(), Value::Object(tel));
                self.last_eot = Some(Value::Object(le));
            }
        }
        let event = json!({"start_ts": ev.start_ts, "end_ts": ev.end_ts, "event_id": ev.event_id});
        if let Err(e) = rec.save(&samples, t_first, &event, ev.peak_snr, &packets) {
            log::warn!("{}: could not save burst: {}", self.name, e);
        }
    }

    // transcription test tap ------------------------------------------------------------

    /// Tap the reference station for `seconds` without retuning; done(audio) once.
    pub fn record_reference(&mut self, seconds: f64, done: Box<dyn FnOnce(Vec<f32>) + Send>) -> bool {
        let Some(r) = &self.plan.reference else { return false };
        if !self.streaming {
            return false;
        }
        let tap = AudioTap::new(self.rate, (freq_of(r) - self.center.unwrap_or(0.0)) * 1e6);
        self.extra_taps.push(ExtraTap { tap, seconds, done: Some(done) });
        true
    }

    fn feed_extra_taps(&mut self, samples: &[Complex32]) {
        let mut i = 0;
        while i < self.extra_taps.len() {
            let item = &mut self.extra_taps[i];
            item.tap.feed(samples);
            if item.tap.duration_s() >= item.seconds {
                let mut item = self.extra_taps.remove(i);
                if let Some(done) = item.done.take() {
                    let audio = item.tap.audio();
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| done(audio))).is_err() {
                        log::error!("reference tap callback failed");
                    }
                }
            } else {
                i += 1;
            }
        }
    }

    /// A brief retune would interrupt every channel; not allowed while streaming.
    pub fn measure(&self, _freq: f64) -> Result<Value, String> {
        if self.streaming {
            return Err("reference is outside the SDR window".into());
        }
        Err(format!("{} is not connected", self.name))
    }

    // status ---------------------------------------------------------------------------

    pub fn floors(&self) -> Vec<f64> {
        self.detectors.iter().filter_map(|d| d.floor).collect()
    }

    pub fn status(&self) -> Map<String, Value> {
        let mut dets: Vec<&ChannelDetector> = self.detectors.iter().collect();
        dets.sort_by(|a, b| a.freq.total_cmp(&b.freq));
        let chans: Vec<Value> = dets.iter().map(|d| json!({
            "freq": d.freq, "ch": d.ch, "label": d.label, "kind": d.kind,
            "floor_db": d.floor.map(|f| round_even(f, 1)), "receiving": d.receiving(),
        })).collect();
        let v = json!({
            "name": self.name, "role": self.role, "serial": self.serial, "antenna": self.antenna,
            "connected": self.connected, "corrupt": self.corrupt,
            "plugged_in": self.plugged_in, "usb_port": self.usb_port,
            "since": round_even(self.since, 1), "center_mhz": self.center, "rate": self.rate,
            "gain": self.gain, "window_mhz": self.plan.window,
            "clipping": round_even(self.clip_fraction, 5), "blocks": self.blocks,
            "overruns": self.overruns, "short_reads": self.short_reads, "stalls": self.stalls,
            "reference": self.reference,
            "uncovered": self.plan.uncovered.iter().map(|c| c.get("ch").cloned().unwrap_or(Value::Null)).collect::<Vec<_>>(),
            "channels": chans, "held_for_transcript": self.held(),
            "last_error": self.last_error,
        });
        let mut m = v.as_object().cloned().unwrap_or_default();
        if let Some(itc) = &self.itc {
            m.insert("itc".into(), Value::Object(itc.status()));
        }
        m
    }
}
