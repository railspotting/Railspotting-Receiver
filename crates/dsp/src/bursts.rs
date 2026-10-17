//! Channel shift and decimate to 48 kS/s, and the rolling telemetry burst recorder.

use num_complex::Complex32;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, VecDeque};
use std::f64::consts::PI;
use std::fs;
use std::io::Write;
use std::ops::{Add, Mul};
use std::path::{Path, PathBuf};

use crate::round_even;

pub const OUT_RATE: f64 = 48_000.0;

/// Normalized sinc, `sin(pi x) / (pi x)`.
pub fn sinc(x: f64) -> f64 {
    let y = PI * if x == 0.0 { 1.0e-20 } else { x };
    y.sin() / y
}

/// Symmetric Blackman window of `m` points.
pub fn blackman(m: usize) -> Vec<f64> {
    if m < 2 {
        return vec![1.0; m];
    }
    let d = (m - 1) as f64;
    (0..m).map(|k| {
        let n = (2 * k) as f64 + 1.0 - m as f64;
        0.42 + 0.5 * (PI * n / d).cos() + 0.08 * (2.0 * PI * n / d).cos()
    }).collect()
}

/// Symmetric Hann window of `m` points (zero at both ends).
pub fn hanning(m: usize) -> Vec<f64> {
    if m < 2 {
        return vec![1.0; m];
    }
    let d = (m - 1) as f64;
    (0..m).map(|k| {
        let n = (2 * k) as f64 + 1.0 - m as f64;
        0.5 + 0.5 * (PI * n / d).cos()
    }).collect()
}

/// Blackman-windowed sinc low-pass with unity DC gain.
pub fn lowpass(num_taps: usize, cutoff_hz: f64, rate: f64) -> Vec<f64> {
    let w = blackman(num_taps);
    let c = (num_taps as f64 - 1.0) / 2.0;
    let h: Vec<f64> = (0..num_taps).map(|k| sinc(2.0 * cutoff_hz / rate * (k as f64 - c)) * w[k]).collect();
    let s: f64 = h.iter().sum();
    h.into_iter().map(|v| v / s).collect()
}

/// Difference of two windowed-sinc low-passes.
pub fn bandpass(num_taps: usize, lo_hz: f64, hi_hz: f64, rate: f64) -> Vec<f64> {
    let hi = lowpass(num_taps, hi_hz, rate);
    let lo = lowpass(num_taps, lo_hz, rate);
    hi.iter().zip(lo).map(|(a, b)| a - b).collect()
}

/// Full linear convolution of `a` and `b` (length `a + b - 1`).
pub fn convolve_full(a: &[f64], b: &[f64]) -> Vec<f64> {
    if a.is_empty() || b.is_empty() {
        return vec![];
    }
    let mut out = vec![0.0; a.len() + b.len() - 1];
    for (i, x) in a.iter().enumerate() {
        for (j, y) in b.iter().enumerate() {
            out[i + j] += x * y;
        }
    }
    out
}

pub fn boxcar3(length: usize) -> Vec<f64> {
    let b = vec![1.0 / length as f64; length];
    convolve_full(&convolve_full(&b, &b), &b)
}

/// A sample type the FIR can filter with real float32 taps.
pub trait Sample: Copy + Default + Add<Output = Self> + Mul<f32, Output = Self> {}
impl Sample for f32 {}
impl Sample for Complex32 {}

/// FIR filter plus decimation that is continuous across arbitrary block sizes.
///
/// Outputs the samples whose absolute input index is a multiple of `decim`,
/// keeping the filter history and the decimation phase between calls.
pub struct FirDecimator<T: Sample> {
    h: Vec<f32>, // reversed taps
    pub decim: usize,
    hist: Vec<T>,
    n0: u64, // absolute index of the first sample of the next block
    buf: Vec<T>,
}

impl<T: Sample> FirDecimator<T> {
    pub fn new(taps: &[f64], decim: usize) -> Self {
        let h: Vec<f32> = taps.iter().rev().map(|&v| v as f32).collect();
        let m = h.len();
        FirDecimator { h, decim: decim.max(1), hist: vec![T::default(); m.saturating_sub(1)], n0: 0, buf: Vec::new() }
    }

    pub fn process(&mut self, x: &[T]) -> Vec<T> {
        let mut out = Vec::new();
        self.process_into(x, &mut out);
        out
    }

    /// Appends the outputs for `x` to `out`.
    pub fn process_into(&mut self, x: &[T], out: &mut Vec<T>) {
        if x.is_empty() {
            return;
        }
        let m = self.h.len();
        let d = self.decim as u64;
        self.buf.clear();
        self.buf.extend_from_slice(&self.hist);
        self.buf.extend_from_slice(x);
        let first = self.n0.div_ceil(d) * d; // ceil to a multiple of decim
        let mut s = (first - self.n0) as usize;
        out.reserve(x.len() / self.decim + 1);
        let h = &self.h[..];
        while s < x.len() {
            let w = &self.buf[s..s + m];
            let mut acc = T::default();
            for (v, &c) in w.iter().zip(h) {
                acc = acc + *v * c;
            }
            out.push(acc);
            s += self.decim;
        }
        self.n0 += x.len() as u64;
        self.hist.clear();
        if m > 1 {
            self.hist.extend_from_slice(&self.buf[self.buf.len() - (m - 1)..]);
        }
    }
}

/// Phase-continuous oscillator that moves `offset_hz` to 0 Hz.
///
/// Uses a cached oscillator table per block length and one complex rotation
/// per block, which is far cheaper than calling exp() for every sample.
pub struct Mixer {
    pub w: f64,
    pub phase: f64,
    tables: HashMap<usize, Vec<Complex32>>,
}

impl Mixer {
    pub fn new(offset_hz: f64, rate: f64) -> Self {
        Mixer { w: -2.0 * PI * offset_hz / rate, phase: 0.0, tables: HashMap::new() }
    }

    pub fn process(&mut self, x: &[Complex32]) -> Vec<Complex32> {
        let mut out = Vec::new();
        self.process_into(x, &mut out);
        out
    }

    /// Replaces the contents of `out` with the mixed block.
    pub fn process_into(&mut self, x: &[Complex32], out: &mut Vec<Complex32>) {
        let n = x.len();
        if !self.tables.contains_key(&n) {
            if self.tables.len() > 8 {
                self.tables.clear();
            }
            let w = self.w;
            self.tables.insert(n, (0..n).map(|k| {
                let a = w * k as f64;
                Complex32::new(a.cos() as f32, a.sin() as f32)
            }).collect());
        }
        let table = &self.tables[&n];
        let rot = Complex32::new(self.phase.cos() as f32, self.phase.sin() as f32);
        out.clear();
        out.extend(x.iter().zip(table).map(|(v, t)| v * (t * rot)));
        self.phase = (self.phase + self.w * n as f64).rem_euclid(2.0 * PI);
    }
}

/// Shift a channel to 0 Hz and decimate to 48 kS/s in two stages.
pub struct ShiftDecimator {
    pub rate: f64,
    pub mixer: Mixer,
    pub stage1: FirDecimator<Complex32>,
    pub stage2: FirDecimator<Complex32>,
    pub out_rate: f64,
    mixed: Vec<Complex32>,
    s1: Vec<Complex32>,
}

impl ShiftDecimator {
    pub fn new(rate: f64, offset_hz: f64) -> Self {
        Self::with_out_rate(rate, offset_hz, OUT_RATE)
    }

    pub fn with_out_rate(rate: f64, offset_hz: f64, out_rate: f64) -> Self {
        let d2 = ((rate / 10.0 / out_rate).round_ties_even() as usize).max(1);
        ShiftDecimator {
            rate,
            mixer: Mixer::new(offset_hz, rate),
            stage1: FirDecimator::new(&boxcar3(10), 10),
            stage2: FirDecimator::new(&lowpass(63, 12_000.0, rate / 10.0), d2),
            out_rate: rate / 10.0 / d2 as f64,
            mixed: Vec::new(),
            s1: Vec::new(),
        }
    }

    pub fn process(&mut self, x: &[Complex32]) -> Vec<Complex32> {
        let mut out = Vec::new();
        self.process_into(x, &mut out);
        out
    }

    /// Appends the 48 kS/s output for `x` to `out`.
    pub fn process_into(&mut self, x: &[Complex32], out: &mut Vec<Complex32>) {
        self.mixer.process_into(x, &mut self.mixed);
        self.s1.clear();
        self.stage1.process_into(&self.mixed, &mut self.s1);
        self.stage2.process_into(&self.s1, out);
    }
}

/// Instantaneous phase step in radians; `prev` continues from the last block.
pub fn fm_discriminate(y: &[Complex32], prev: Option<Complex32>) -> Vec<f32> {
    let mut out = Vec::with_capacity(y.len());
    let mut last = match prev {
        Some(p) => p,
        None => match y.first() {
            Some(&f) => f,
            None => return out,
        },
    };
    let rest = if prev.is_some() { y } else { &y[1..] };
    for &v in rest {
        out.push((v * last.conj()).arg());
        last = v;
    }
    out
}

/// Local time as `%Y%m%d-%H%M%S` (UTC where the platform has no local time).
pub(crate) fn local_stamp(t: f64) -> String {
    let secs = t.floor() as i64;
    #[cfg(unix)]
    {
        let tt = secs as libc::time_t;
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        // SAFETY: localtime_r only writes the tm we hand it
        let ok = unsafe { !libc::localtime_r(&tt, &mut tm).is_null() };
        if ok {
            return format!("{:04}{:02}{:02}-{:02}{:02}{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday,
                           tm.tm_hour, tm.tm_min, tm.tm_sec);
        }
    }
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!("{:04}{:02}{:02}-{:02}{:02}{:02}", y, m, d, rem / 3600, rem % 3600 / 60, rem % 60)
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// The key order of a decoded EOT packet (`eot::fields`, then `t_offset_s`).
fn packet_key_rank(k: &str) -> usize {
    const ORDER: [&str; 17] = ["protocol", "chaining", "battery_condition", "message_type", "unit_id",
                               "brake_pipe_psi", "battery_charge_raw", "valve_circuit_ok", "confirmation",
                               "turbine", "motion", "marker_battery_weak", "marker_light", "decoder", "crc_ok",
                               "corrected_bits", "t_offset_s"];
    ORDER.iter().position(|o| *o == k).unwrap_or(ORDER.len())
}

/// Indented JSON (2 spaces) of `v` at nesting `level`; object keys in packet order, then the rest.
fn dump_value(v: &Value, level: usize, out: &mut String) {
    let pad = |n: usize| "  ".repeat(n);
    match v {
        Value::Array(a) if !a.is_empty() => {
            out.push_str("[\n");
            for (i, x) in a.iter().enumerate() {
                out.push_str(&pad(level + 1));
                dump_value(x, level + 1, out);
                out.push_str(if i + 1 < a.len() { ",\n" } else { "\n" });
            }
            out.push_str(&pad(level));
            out.push(']');
        }
        Value::Object(o) if !o.is_empty() => {
            let mut keys: Vec<&String> = o.keys().collect();
            keys.sort_by_key(|k| packet_key_rank(k)); // stable: unknown keys stay alphabetical
            out.push_str("{\n");
            for (i, k) in keys.iter().enumerate() {
                out.push_str(&pad(level + 1));
                out.push_str(&Value::String((*k).clone()).to_string());
                out.push_str(": ");
                dump_value(&o[*k], level + 1, out);
                out.push_str(if i + 1 < keys.len() { ",\n" } else { "\n" });
            }
            out.push_str(&pad(level));
            out.push('}');
        }
        Value::Number(n) if n.is_f64() => out.push_str(&value_f64(n.as_f64().unwrap_or(f64::NAN))),
        v => out.push_str(&v.to_string()),
    }
}

/// A float as the burst JSON writes it: shortest round-trip digits, always with a `.0` or an
/// exponent, and exponent form below 1e-4 and from 1e16 (`1e-05`, `1e+16`).
pub(crate) fn value_f64(x: f64) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    let sci = format!("{:e}", x); // shortest round-trip digits, e.g. "-1.2345e-5"
    let (mant, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let (sign, mant) = mant.strip_prefix('-').map_or(("", mant), |m| ("-", m));
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let decpt = exp + 1;
    if -4 < decpt && decpt <= 16 {
        let n = digits.len() as i32;
        let body = if decpt <= 0 {
            format!("0.{}{}", "0".repeat((-decpt) as usize), digits)
        } else if decpt >= n {
            format!("{}{}.0", digits, "0".repeat((decpt - n) as usize))
        } else {
            format!("{}.{}", &digits[..decpt as usize], &digits[decpt as usize..])
        };
        return format!("{sign}{body}");
    }
    let rest = if digits.len() > 1 { format!(".{}", &digits[1..]) } else { String::new() };
    format!("{sign}{}{rest}e{}{:02}", &digits[..1], if exp < 0 { '-' } else { '+' }, exp.abs())
}

/// ASCII-only JSON text: every character outside space to `~` as a `\uXXXX` escape. Control
/// characters are already escaped by serde_json, so only DEL and non-ASCII are left here.
fn ascii_only(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii() && c != '\x7f' {
            out.push(c);
        } else {
            let mut units = [0u16; 2];
            for u in c.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{:04x}", u));
            }
        }
    }
    out
}

/// Indented JSON (2 spaces, ASCII only) with the top-level keys in the order given.
pub(crate) fn pretty_ordered(pairs: &[(&str, Value)]) -> String {
    let mut s = String::from("{\n");
    for (i, (k, v)) in pairs.iter().enumerate() {
        s += &format!("  {}: ", Value::String((*k).into()));
        dump_value(v, 1, &mut s);
        s += if i + 1 < pairs.len() { ",\n" } else { "\n" };
    }
    ascii_only(&(s + "}"))
}

/// Keeps the last few seconds of one channel at 48 kS/s and saves bursts.
pub struct BurstRecorder {
    pub sd: ShiftDecimator,
    pub out_rate: f64,
    pub freq_mhz: f64,
    pub label: String,
    pub ch: Value,
    pub out_dir: Option<PathBuf>,
    pub keep: usize,
    pub enabled: bool,
    pub center_mhz: Option<f64>,
    pub max_len: usize,
    pub buf: VecDeque<Complex32>,
    pub t_last: Option<f64>, // time of the newest sample
    y: Vec<Complex32>,
}

impl BurstRecorder {
    #[allow(clippy::too_many_arguments)]
    pub fn new(rate: f64, offset_hz: f64, freq_mhz: f64, label: &str, ch: Value, out_dir: Option<&Path>,
               keep: usize, buffer_s: f64, center_mhz: Option<f64>, enabled: bool) -> Self {
        let sd = ShiftDecimator::new(rate, offset_hz);
        let out_rate = sd.out_rate;
        BurstRecorder {
            sd, out_rate, freq_mhz, label: label.into(), ch, out_dir: out_dir.map(Path::to_path_buf), keep,
            enabled, center_mhz, max_len: (buffer_s * out_rate) as usize, buf: VecDeque::new(), t_last: None,
            y: Vec::new(),
        }
    }

    pub fn feed(&mut self, block: &[Complex32], ts_end: f64) {
        self.y.clear();
        self.sd.process_into(block, &mut self.y);
        if self.y.is_empty() {
            return;
        }
        self.buf.extend(self.y.iter().copied());
        while self.buf.len() > self.max_len {
            self.buf.pop_front();
        }
        self.t_last = Some(ts_end);
    }

    pub fn reset(&mut self) {
        self.buf.clear();
        self.t_last = None;
    }

    /// Samples between t0 and t1 (clipped to the buffer) and the first sample's time.
    pub fn extract(&self, t0: f64, t1: f64) -> (Vec<Complex32>, f64) {
        let t_last = match self.t_last {
            Some(t) if !self.buf.is_empty() => t,
            _ => return (vec![], t0),
        };
        let t_first = t_last - (self.buf.len() as f64 - 1.0) / self.out_rate;
        let i0 = ((t0 - t_first) * self.out_rate).round_ties_even().max(0.0) as usize;
        let i1 = ((((t1 - t_first) * self.out_rate).round_ties_even() as i64) + 1).clamp(0, self.buf.len() as i64) as usize;
        if i1 <= i0 {
            return (vec![], t0);
        }
        (self.buf.range(i0..i1).copied().collect(), t_first + i0 as f64 / self.out_rate)
    }

    /// Writes `<stamp>_<freq>.cf32` and its `.json`. `event` carries start_ts, end_ts, event_id.
    pub fn save(&self, samples: &[Complex32], t_first: f64, event: &Value, snr_peak_db: f64,
                decoded: &[Map<String, Value>]) -> std::io::Result<Option<PathBuf>> {
        let out_dir = match &self.out_dir {
            Some(d) if self.enabled && !samples.is_empty() => d,
            _ => return Ok(None),
        };
        fs::create_dir_all(out_dir)?;
        let truthy = |v: Option<f64>| v.filter(|x| *x != 0.0);
        let start_v = event.get("start_ts").and_then(Value::as_f64);
        let end_v = event.get("end_ts").and_then(Value::as_f64);
        let start = truthy(start_v).unwrap_or(t_first);
        let stamp = format!("{}.{:03}", local_stamp(start), ((start * 1000.0) as i64).rem_euclid(1000));
        let base = format!("{}_{:.4}", stamp, self.freq_mhz);
        let cf32 = out_dir.join(format!("{base}.cf32"));
        let mut bytes = Vec::with_capacity(samples.len() * 8);
        for s in samples {
            bytes.extend_from_slice(&s.re.to_le_bytes());
            bytes.extend_from_slice(&s.im.to_le_bytes());
        }
        fs::write(&cf32, bytes)?;
        let meta = [
            ("freq_mhz", json!(self.freq_mhz)), ("label", json!(self.label)), ("ch", self.ch.clone()),
            ("sample_rate", json!(self.out_rate)), ("format", json!("cf32_le")),
            ("samples", json!(samples.len())), ("t_first_sample", json!(t_first)),
            ("burst_start_ts", event.get("start_ts").cloned().unwrap_or(Value::Null)),
            ("burst_end_ts", event.get("end_ts").cloned().unwrap_or(Value::Null)),
            ("duration_s", json!(round_even(truthy(end_v).unwrap_or(start) - start, 3))),
            ("snr_peak_db", json!(round_even(snr_peak_db, 1))),
            ("event_id", event.get("event_id").cloned().unwrap_or(Value::Null)),
            ("sdr_center_mhz", json!(self.center_mhz)),
            ("decoded", Value::Array(decoded.iter().cloned().map(Value::Object).collect())),
        ];
        let mut f = fs::File::create(out_dir.join(format!("{base}.json")))?;
        f.write_all((pretty_ordered(&meta) + "\n").as_bytes())?;
        self.prune();
        Ok(Some(cf32))
    }

    pub fn prune(&self) {
        let Some(dir) = &self.out_dir else { return };
        let Ok(rd) = fs::read_dir(dir) else { return };
        let mut files: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "cf32")).collect();
        files.sort();
        let n = files.len().saturating_sub(self.keep);
        for old in &files[..n] {
            let _ = fs::remove_file(old);
            let _ = fs::remove_file(old.with_extension("json"));
        }
    }
}
