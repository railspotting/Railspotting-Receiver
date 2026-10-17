//! Known defect detector announcements, recognized from their sound.
//!
//! A wayside defect detector plays the same synthetic opening every time ("Norfolk Southern,
//! milepost 475.8") and, when the train is fine, the same ending ("no defects, repeat, no defects").
//! A template made from a few recordings (log band energies, aligned and averaged so radio noise
//! cancels) is compared with each voice call before speech recognition, in two stages:
//!
//! 1. The opening identifies the detector. When it matches, the call is a message from this
//!    detector, whatever follows. Detecting the message matters more than its words.
//! 2. The rest is compared with the all-clear ending. A match gives the template's exact text and
//!    parsed fields. Anything else is a different message from the detector (possibly a defect):
//!    the rest is transcribed, and the result is never reported as "no defects".
//!
//! Both stages cut their part of the template into pieces, and every piece must match on its own.
//! An average alone is not safe: a real opening followed by other speech scored up to 0.52 against
//! the whole announcement. Measured on the reference detector's recordings (NS milepost 475.8), with templates made without the clip tested:
//! - opening (4 pieces): every piece after the first 0.48 or more for the detector; each of 43 other
//!   calls had a piece at 0.30 or less, or didn't reach the end of the opening;
//! - ending (3 pieces) and the whole-announcement check: see the README.
//!
//! The first opening piece may be missing (call caught late). The ending's pieces never may.
//!
//! Templates live in state/detector_templates/<name>.json + <name>.npy and are made with
//! `railspotting-receiver detector add`.

use crate::np::{self, round_even, clamp_slice, round_int, sum_f32, sum_f64};
use crate::npy;
use crate::jsonfmt;
use num_complex::Complex;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::SystemTime;

pub const RATE: usize = 16_000;
pub const FFT: usize = 512;
pub const HOP: usize = 320; // 20 ms frames
pub const N_BANDS: usize = 24;
pub const ENGINE: &str = "railspotting-detector-match-1";
pub const ALIGN_FRAMES: i64 = 25; // how far recordings may be shifted against each other when averaging
pub const MAX_LEAD_S: f64 = 1.0; // a call may start up to this late into the announcement
pub const PIECE_SLACK: i64 = 3; // frames each piece may shift against the stage's alignment
pub const ENDING_SLACK: i64 = 10; // frames the ending may shift against where the opening put it

// Length and rhythm. The reference detector's all-clear is 9.2 s of radio every time (9.7 s clips with the
// pre-roll), sent twice about 20 s apart. A message from the detector longer than its all-clear
// is never taken for it, whatever the sound match says. When the ending's detailed check just
// misses (it held to about 5 dB of added noise on real recordings), the rhythm of the ending (its
// loud and quiet pattern, 100 ms smoothed) can confirm it, for near misses only. Real all-clears
// with 0 to 3 dB of added noise that the detailed check missed: rhythm 0.94 to 0.97, every ending
// piece 0.40 or more. Test messages that kept most of a real all-clear and replaced its last
// second or more with other recorded speech passed a 0.70 rhythm limit (0.71 to 0.75) but had an
// ending piece of 0.18 to 0.34. With both limits below, none of 3,870 such messages passes.
pub const LONGER_MAX_S: f64 = 0.3; // how much longer than its all-clear a call may be and still be one
pub const SHORTER_MAX_S: f64 = 1.0; // ... and shorter (caught late), for the rhythm check
pub const RHYTHM_MIN: f64 = 0.90;
pub const RHYTHM_PIECE_MIN: f64 = 0.38; // every ending piece, against ENDING_MIN for the detailed check
pub const RHYTHM_SMOOTH: usize = 5; // frames (100 ms)

// Templates with the milepost left out of matching (opening_end_s): the opening is only the
// detector system's wording ("Norfolk Southern, milepost"), the ending ("no defects. Repeat, no
// defects." up to the transmitter's key-off) is searched for wherever the number leaves it, and
// the number is checked against the template's own only to say whether the milepost is known.
// Measured on the reference detector (templates made without the clip tested): the wording in 4 pieces scored
// 0.47 or more (weakest piece after the first) for real announcements and 0.13 at most for 43
// other calls; the ending, found anywhere, 0.53 or more against 0.11 for other calls and 0.19
// for the real opening followed by other speech. Real announcements with other audio of 1 to
// 3 s in place of the number were all recognized wherever the wording was heard.
pub const OPENING_PIECES: usize = 4;
pub const OPENING_MIN: f64 = 0.30;
pub const ENDING_PIECES: usize = 3;
pub const ENDING_MIN: f64 = 0.40;
pub const MILEPOST_PIECES: usize = 2;
pub const MILEPOST_MIN: f64 = 0.40;
pub const NUMBER_MIN_S: f64 = 0.5; // how long a spoken milepost number may take
pub const NUMBER_MAX_S: f64 = 4.5;
pub const NUMBER_SLACK: i64 = 15; // frames: the template's own number, said at its length
pub const AFTER_MAX_S: f64 = 0.3; // anything longer after the ending is more message
pub const END_SLACK: i64 = 15; // frames: the ending finishes this close to the end of the call
// templates with only a split (opening includes the number, ending at a fixed place)
pub const LEGACY_OPENING_MIN: f64 = 0.40;
pub const LEGACY_ENDING_MIN: f64 = 0.45;
// templates made before the split existed: the whole announcement in 6 pieces
pub const WHOLE_PIECES: usize = 6;
pub const WHOLE_MIN: f64 = 0.45;
pub const MATCH_MIN: f64 = 0.45;

fn name_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^(?:[a-z0-9][a-z0-9._-]{0,40})\z").expect("name regex"))
}

/// `NAME_RE.fullmatch(name)`: template names and detector IDs.
pub fn valid_name(name: &str) -> bool {
    name_re().is_match(name)
}

// features ----------------------------------------------------------------------------------------

/// Frames of band energies: `rows` x `cols` float32 in C order (row after row).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Feats {
    pub data: Vec<f32>,
    pub cols: usize,
}

impl Feats {
    pub fn new(data: Vec<f32>, cols: usize) -> Self {
        Feats { data, cols }
    }
    pub fn len(&self) -> usize {
        self.view().len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn view(&self) -> FView<'_> {
        FView { data: &self.data, cols: self.cols }
    }
    pub fn row(&self, i: usize) -> &[f32] {
        &self.data[i * self.cols..(i + 1) * self.cols]
    }
}

/// A run of frames (rows) of a `Feats`.
#[derive(Debug, Clone, Copy)]
pub struct FView<'a> {
    pub data: &'a [f32],
    pub cols: usize,
}

impl<'a> FView<'a> {
    pub fn len(&self) -> usize {
        self.data.len().checked_div(self.cols).unwrap_or(0)
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Rows `[start:stop]`, clamped, negative bounds counting from the end.
    pub fn slice(&self, start: Option<i64>, stop: Option<i64>) -> FView<'a> {
        let (a, b) = clamp_slice(self.len(), start, stop);
        FView { data: &self.data[a * self.cols..b * self.cols], cols: self.cols }
    }
    pub fn to_feats(&self) -> Feats {
        Feats { data: self.data.to_vec(), cols: self.cols }
    }
}

struct Bands {
    hann: Vec<f64>,
    edges: Vec<(usize, usize)>, // FFT bins of each band, [lo, hi)
    speech: (usize, usize),     // BANDS[0] to BANDS[-1]
}

fn bands() -> &'static Bands {
    static B: OnceLock<Bands> = OnceLock::new();
    B.get_or_init(|| {
        let b = np::geomspace(300.0, 3400.0, N_BANDS + 1);
        let freqs = np::rfftfreq(FFT, 1.0 / RATE as f64);
        let bins = |lo: f64, hi: f64| {
            let idx: Vec<usize> = (0..freqs.len()).filter(|&i| freqs[i] >= lo && freqs[i] < hi).collect();
            match (idx.first(), idx.last()) {
                (Some(&a), Some(&z)) => (a, z + 1),
                _ => (0, 0),
            }
        };
        Bands {
            hann: np::hanning(FFT),
            edges: (0..N_BANDS).map(|k| bins(b[k], b[k + 1])).collect(),
            speech: bins(b[0], b[N_BANDS]),
        }
    })
}

/// Power spectrum of every 20 ms frame through a Hann window, `|rfft(frame * hann)|^2` (float64).
fn frame_power(audio: &[f32]) -> Vec<Vec<f64>> {
    let mut x = audio.to_vec();
    if x.len() < FFT {
        x.resize(FFT, 0.0);
    }
    let hann = &bands().hann;
    let mut fft = np::Rfft64::new(FFT);
    let mut frame = vec![0.0f64; FFT];
    let n = (x.len() - FFT) / HOP + 1;
    (0..n)
        .map(|k| {
            for (i, v) in frame.iter_mut().enumerate() {
                *v = x[k * HOP + i] as f64 * hann[i];
            }
            fft.power(&frame)
        })
        .collect()
}

/// Log energy in 24 bands from 300 to 3400 Hz every 20 ms, each band normalized.
pub fn features(audio: &[f32]) -> Feats {
    let power = frame_power(audio);
    let b = bands();
    let rows = power.len();
    let mut out = vec![0.0f64; rows * N_BANDS];
    for (r, p) in power.iter().enumerate() {
        for (k, &(lo, hi)) in b.edges.iter().enumerate() {
            let s = sum_f64(hi - lo, &|i| p[lo + i]);
            out[r * N_BANDS + k] = (s + 1e-10).ln();
        }
    }
    let n = rows as f64;
    for k in 0..N_BANDS {
        let mut s = 0.0;
        for r in 0..rows {
            s += out[r * N_BANDS + k];
        }
        let mean = s / n;
        for r in 0..rows {
            out[r * N_BANDS + k] -= mean;
        }
        // standard deviation: its own mean of the centred values, then the mean squared deviation
        let mut s2 = 0.0;
        for r in 0..rows {
            s2 += out[r * N_BANDS + k];
        }
        let m2 = s2 / n;
        let mut v = 0.0;
        for r in 0..rows {
            let d = out[r * N_BANDS + k] - m2;
            v += d * d;
        }
        let sd = (v / n).sqrt() + 1e-6;
        for r in 0..rows {
            out[r * N_BANDS + k] /= sd;
        }
    }
    Feats { data: out.into_iter().map(|v| v as f32).collect(), cols: N_BANDS }
}

/// Correlation of two equal runs of frames, in float32 with pairwise sums.
fn corr(a: &[f32], b: &[f32]) -> f64 {
    let n = a.len().min(b.len());
    let aa = sum_f32(n, &|i| a[i] * a[i]);
    let bb = sum_f32(n, &|i| b[i] * b[i]);
    let d = (aa * bb).sqrt() as f64;
    let ab = sum_f32(n, &|i| a[i] * b[i]) as f64;
    if d > 0.0 { ab / d } else { 0.0 }
}

/// Best correlation of the template against the call and its offset (frames of the call
/// where template frame 0 lands; negative when the call started late).
pub fn align(template: FView, call: FView, lead: i64, min_overlap: f64, offsets: Option<Range<i64>>)
    -> (f64, Option<i64>) {
    let t_len = template.len() as i64;
    let c_len = call.len() as i64;
    let (mut best, mut best_off) = (0.0, None);
    let need = min_overlap * t_len as f64;
    let offsets = offsets.unwrap_or(-lead..c_len - need as i64 + 1);
    for off in offsets {
        let (t0, c0) = (0.max(-off), 0.max(off));
        let n = (t_len - t0).min(c_len - c0);
        if (n as f64) < need || n < 0 {
            continue;
        }
        let c = corr(template.slice(Some(t0), Some(t0 + n)).data, call.slice(Some(c0), Some(c0 + n)).data);
        if c > best {
            best = c;
            best_off = Some(off);
        }
    }
    (best, best_off)
}

pub fn match_score(template: FView, call: FView) -> f64 {
    align(template, call, lead_frames(), 0.8, None).0
}

fn lead_frames() -> i64 {
    (MAX_LEAD_S * RATE as f64 / HOP as f64) as i64
}

fn frames_of(s: f64) -> i64 {
    (s * RATE as f64 / HOP as f64) as i64
}

/// Each of n pieces of the template at the given alignment; None where the call doesn't
/// reach that piece.
pub fn pieces(template: FView, call: FView, offset: Option<i64>, n: usize) -> Vec<Option<f64>> {
    let Some(offset) = offset else { return vec![None; n] };
    let t_len = template.len() as i64;
    let c_len = call.len() as i64;
    (0..n as i64)
        .map(|k| {
            let (a, b) = (k * t_len / n as i64, (k + 1) * t_len / n as i64);
            let mut best: Option<f64> = None;
            for d in -PIECE_SLACK..=PIECE_SLACK {
                let (ca, cb) = (a + offset + d, b + offset + d);
                if ca >= 0 && cb <= c_len {
                    let c = corr(template.slice(Some(a), Some(b)).data, call.slice(Some(ca), Some(cb)).data);
                    best = Some(match best { Some(x) if c > x => c, Some(x) => x, None => c });
                }
            }
            best
        })
        .collect()
}

fn r2(xs: &[Option<f64>]) -> Vec<Option<f64>> {
    xs.iter().map(|x| x.map(|v| round_even(v, 2))).collect()
}

/// How a call compares with a template.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Assessment {
    /// "all_clear", "other", "incomplete" or None
    pub kind: Option<String>,
    /// "sound", "ending", "rhythm and length" or None
    pub method: Option<String>,
    pub score: f64,
    pub opening: Option<Vec<Option<f64>>>,
    pub ending: Option<Vec<Option<f64>>>,
    /// "known", "other" or None
    pub milepost: Option<String>,
    pub number_frames: Option<(Option<i64>, Option<i64>)>,
    pub rest_frame: Option<i64>,
    pub opening_frame: Option<i64>,
    /// set only on an all-clear by sound (the JSON key is absent otherwise)
    pub ending_frame: Option<i64>,
    /// set by `check_length_and_rhythm` when the template knows its length
    pub length_s: Option<f64>,
    /// set by `check_length_and_rhythm` when the rhythm was judged
    pub rhythm: Option<f64>,
}

impl Assessment {
    pub fn kind(&self) -> Option<&str> {
        self.kind.as_deref()
    }
    pub fn is(&self, kind: &str) -> bool {
        self.kind.as_deref() == Some(kind)
    }

    /// The assessment as JSON, keys in the fixed reference order.
    pub fn to_json(&self) -> Value {
        let list = |v: &Option<Vec<Option<f64>>>| match v {
            Some(xs) => Value::Array(xs.iter().map(|x| x.map(f64_value).unwrap_or(Value::Null)).collect()),
            None => Value::Null,
        };
        let mut m = Map::new();
        m.insert("kind".into(), json!(self.kind));
        m.insert("method".into(), json!(self.method));
        m.insert("score".into(), f64_value(self.score));
        m.insert("opening".into(), list(&self.opening));
        m.insert("ending".into(), list(&self.ending));
        m.insert("milepost".into(), json!(self.milepost));
        m.insert("number_frames".into(), match self.number_frames {
            Some((a, b)) => json!([a, b]),
            None => Value::Null,
        });
        m.insert("rest_frame".into(), json!(self.rest_frame));
        m.insert("opening_frame".into(), json!(self.opening_frame));
        if let Some(e) = self.ending_frame {
            m.insert("ending_frame".into(), json!(e));
        }
        if let Some(l) = self.length_s {
            m.insert("length_s".into(), f64_value(l));
        }
        if let Some(r) = self.rhythm {
            m.insert("rhythm".into(), f64_value(r));
        }
        Value::Object(m)
    }
}

fn f64_value(x: f64) -> Value {
    serde_json::Number::from_f64(x).map(Value::Number).unwrap_or(Value::Null)
}

fn all_some_at_least(xs: &[Option<f64>], min: f64) -> bool {
    xs.iter().all(|p| matches!(p, Some(v) if *v >= min))
}

/// How a call compares with a template.
///
/// kind: "all_clear" (the standard ending was heard and nothing followed it), "other" (the
/// opening was heard but not the all-clear: a different message, possibly a defect report),
/// "incomplete" (the opening was heard but the call stopped before an all-clear could fit), or
/// None. method: "sound" (opening and ending), "ending" (the ending alone, opening not heard).
/// milepost: "known" when the template's own number was heard between them, "other" when
/// something else was said there (its frames are in number_frames, for speech recognition),
/// None when not judged. Frame positions are in the call's features.
pub fn assess(template: FView, call: FView, split: Option<i64>, opening_end: Option<i64>) -> Assessment {
    let lead = lead_frames();
    let mut out = Assessment::default();
    let split = match split {
        Some(s) if s != 0 => s,
        _ => {
            let (overall, off) = align(template, call, lead, 0.8, None);
            let p = pieces(template, call, off, WHOLE_PIECES);
            let ok = overall >= MATCH_MIN
                && p[1..].iter().all(Option::is_some)
                && p.iter().flatten().all(|v| *v >= WHOLE_MIN);
            out.kind = ok.then(|| "all_clear".into());
            out.method = ok.then(|| "sound".into());
            out.score = round_even(overall, 2);
            out.ending = Some(r2(&p));
            out.milepost = ok.then(|| "known".into());
            return out;
        }
    };
    let legacy = !matches!(opening_end, Some(o) if o != 0);
    let a_end = if legacy { split } else { opening_end.unwrap_or(0) };
    let opening = template.slice(None, Some(a_end));
    let number = template.slice(Some(a_end), Some(split));
    let ending = template.slice(Some(split), None);
    let (o_min, e_min) = if legacy { (LEGACY_OPENING_MIN, LEGACY_ENDING_MIN) } else { (OPENING_MIN, ENDING_MIN) };

    // 1. the wording that starts every message from this kind of detector
    let (o_score, o_off) = align(opening, call, lead, 0.8, Some(-lead..lead + 1));
    let o_pieces = pieces(opening, call, o_off, OPENING_PIECES);
    let checked = if legacy { &o_pieces[..] } else { &o_pieces[1..] }; // a late start may spoil the first
    let heard = o_off.is_some()
        && o_pieces[1..].iter().all(Option::is_some)
        && checked.iter().flatten().all(|p| *p >= o_min);
    out.score = round_even(o_score, 2);
    out.opening = Some(r2(&o_pieces));

    // 2. the standard ending. With the number left out of matching, it is timed from the end of
    // the call: the ending runs up to the transmitter's key-off, so it sits at the call's end
    // whatever the milepost number's length ("end-x timing"); after the number when the opening
    // was heard. Templates with only a split keep the ending at its fixed place.
    let c_len = call.len() as i64;
    let e_len = ending.len() as i64;
    let last = c_len - e_len;
    let o_off_v = o_off.unwrap_or(0);
    let start = o_off_v + a_end;
    if heard {
        out.opening_frame = Some(o_off_v);
        out.rest_frame = Some(0.max(start));
    }
    let offs = if legacy {
        if heard { start - ENDING_SLACK..start + ENDING_SLACK + 1 } else { 0..0 }
    } else {
        let mut first = last - END_SLACK;
        if heard {
            first = first.max(start + frames_of(NUMBER_MIN_S));
        }
        0.max(first)..last + 1
    };
    let (e_score, e_off) = align(ending, call, 0, 1.0, Some(offs));
    let e_pieces = pieces(ending, call, e_off, ENDING_PIECES);
    let found = all_some_at_least(&e_pieces, e_min);
    out.ending = Some(r2(&e_pieces));
    if !heard && !found {
        return out;
    }
    let e_off_v = e_off.unwrap_or(0);

    // 3. the number between them: the template's own, or another milepost
    if legacy {
        out.milepost = Some("known".into());
    } else if !number.is_empty() {
        let n_len = number.len() as i64;
        let (gap_ok, n_start) = if heard && found {
            ((e_off_v - (o_off_v + a_end) - n_len).abs() <= NUMBER_SLACK, o_off_v + a_end)
        } else if heard {
            (true, o_off_v + a_end)
        } else {
            (true, e_off_v - n_len)
        };
        let n_pieces = if gap_ok { pieces(number, call, Some(n_start), MILEPOST_PIECES) } else { vec![None] };
        let known = all_some_at_least(&n_pieces, MILEPOST_MIN);
        out.milepost = Some(if known { "known" } else { "other" }.into());
        if !known {
            out.number_frames = Some((heard.then_some(o_off_v + a_end), found.then_some(e_off_v)));
        }
    }

    // 4. the verdict
    if found {
        let after = (c_len - (e_off_v + e_len)) as f64 * HOP as f64 / RATE as f64;
        if after > AFTER_MAX_S {
            out.kind = heard.then(|| "other".into());
            return out;
        }
        out.kind = Some("all_clear".into());
        out.method = Some(if heard { "sound" } else { "ending" }.into());
        out.score = round_even(if heard { o_score } else { e_score }, 2);
        out.ending_frame = Some(e_off_v);
        if !heard {
            out.rest_frame = Some(e_off_v);
        }
        return out;
    }
    let shortest = o_off_v + a_end + if legacy { 0 } else { frames_of(NUMBER_MIN_S) } + e_len;
    out.kind = Some(if !legacy && c_len < shortest { "incomplete" } else { "other" }.into());
    out
}

// noise filter trained on the opening -------------------------------------------------------
//
// The opening is a known passage, like the training sequence of a digital radio: the template
// says where its gaps between words fall, so this transmission's hiss can be measured exactly
// there and removed from the unknown rest before speech recognition. The filter knows only how
// noisy each frequency is, nothing about the words that should follow, so it can't make a
// message sound like the all-clear. On the reference detector's six recordings it raised "no defects" heard
// correctly in the ending from 2 to 3 of 6 (4 of 6 in a first experiment); an ordinary filter
// that guesses the quiet parts didn't help (2 of 6). Modest, but it can't mislead.

pub const WIENER_FFT: usize = 512;
pub const WIENER_HOP: usize = 128;
pub const WIENER_OVER: f32 = 1.5; // remove a little more than the measured noise
pub const WIENER_FLOOR: f32 = 0.12; // never cut a frequency by more than about 18 dB
pub const QUIET_SHARE: f64 = 0.25; // the quietest quarter of the opening's frames are its gaps

fn window() -> &'static [f32] {
    static W: OnceLock<Vec<f32>> = OnceLock::new();
    W.get_or_init(|| np::hanning(WIENER_FFT + 1)[..WIENER_FFT].iter().map(|v| v.sqrt() as f32).collect())
}

/// Log energy from 300 to 3400 Hz per 20 ms frame.
fn energy(audio: &[f32], smooth: usize) -> Vec<f64> {
    let (lo, hi) = bands().speech;
    let e: Vec<f64> = frame_power(audio).iter().map(|p| (sum_f64(hi - lo, &|i| p[lo + i]) + 1e-10).ln()).collect();
    if smooth > 1 { np::convolve_same(&e, &vec![1.0 / smooth as f64; smooth]) } else { e }
}

fn minus_median(e: Vec<f64>) -> Vec<f64> {
    let m = np::median(&e);
    e.into_iter().map(|v| v - m).collect()
}

/// The shift (frames) that lines `e` up with `reference` best, by correlation.
fn best_shift(e: &[f64], reference: &[f64]) -> i64 {
    let (mut best, mut shift) = (-2.0, 0);
    for off in -ALIGN_FRAMES..=ALIGN_FRAMES {
        let seg = shifted(e, off, reference.len());
        let (a, _) = clamp_slice(reference.len(), Some(0.max(-off)), None);
        let r = &reference[a..];
        let r = &r[..clamp_slice(r.len(), None, Some(seg.len() as i64)).1];
        let c = np::corrcoef(seg, r);
        if c > best {
            best = c;
            shift = off;
        }
    }
    shift
}

/// `e[max(0, off):][: len(ref) - max(0, -off)]`
fn shifted(e: &[f64], off: i64, ref_len: usize) -> &[f64] {
    let (a, _) = clamp_slice(e.len(), Some(0.max(off)), None);
    let s = &e[a..];
    &s[..clamp_slice(s.len(), None, Some(ref_len as i64 - 0.max(-off))).1]
}

/// Mean of each column over the rows, ignoring NaN (NaN where a column is all NaN).
fn nanmean(rows: &[Vec<f64>], n: usize) -> Vec<f64> {
    (0..n)
        .map(|i| {
            let (mut s, mut k) = (0.0, 0usize);
            for r in rows {
                if !r[i].is_nan() {
                    s += r[i];
                    k += 1;
                }
            }
            if k == 0 { f64::NAN } else { s / k as f64 }
        })
        .collect()
}

/// Each recording's per-frame values aligned to the first's, NaN where it has none.
fn aligned_rows(series: Vec<Vec<f64>>, subtract_seg_median: bool) -> (Vec<Vec<f64>>, usize) {
    let mut reference: Option<Vec<f64>> = None;
    let mut rows = Vec::new();
    for e in series {
        let shift = match &reference {
            None => {
                reference = Some(e.clone());
                0
            }
            Some(r) => best_shift(&e, r),
        };
        let ref_len = reference.as_ref().map(Vec::len).unwrap_or(0);
        let mut row = vec![f64::NAN; ref_len];
        let seg = shifted(&e, shift, ref_len);
        let m = if subtract_seg_median { np::median(seg) } else { 0.0 };
        let at = 0.max(-shift) as usize;
        for (i, v) in seg.iter().enumerate() {
            if at + i < ref_len {
                row[at + i] = v - m;
            }
        }
        rows.push(row);
    }
    let n = reference.map(|r| r.len()).unwrap_or(0);
    (rows, n)
}

/// The announcement's loud and quiet pattern, averaged over the recordings (each aligned to
/// the first).
pub fn rhythm_profile<A: AsRef<[f32]>>(recordings: &[A]) -> Vec<f64> {
    let series = recordings.iter().map(|a| minus_median(energy(a.as_ref(), RHYTHM_SMOOTH))).collect();
    let (rows, n) = aligned_rows(series, false);
    nanmean(&rows, n)
}

/// How well the call's ending rises and falls like the all-clear's (correlation).
pub fn rhythm_score(profile: &[f64], audio: &[f32], split: i64, rest_frame: i64) -> f64 {
    let e = minus_median(energy(audio, RHYTHM_SMOOTH));
    let (a, _) = clamp_slice(profile.len(), Some(split), None);
    let tmpl = &profile[a..];
    let mut best = 0.0;
    for d in -5..=5i64 {
        let (s0, s1) = clamp_slice(e.len(), Some(0.max(rest_frame + d)), Some(rest_frame + d + tmpl.len() as i64));
        let seg = &e[s0..s1];
        let n = seg.len().min(tmpl.len());
        if n as f64 >= 0.9 * tmpl.len() as f64 && np::std(&seg[..n]) > 0.0 {
            let c = np::corrcoef(&seg[..n], &tmpl[..n]);
            if c > best {
                best = c;
            }
        }
    }
    best
}

/// The opening's gaps between words: the quietest frames of the recordings' average loudness
/// (measured before the per-band normalization of the features, which hides loudness).
pub fn quiet_frames<A: AsRef<[f32]>>(recordings: &[A], split: i64) -> Vec<i64> {
    let series = recordings
        .iter()
        .map(|a| frame_power(a.as_ref()).iter().map(|p| (sum_f64(p.len(), &|i| p[i]) + 1e-10).ln()).collect())
        .collect();
    let (rows, n) = aligned_rows(series, true);
    let profile = nanmean(&rows, n);
    let (_, b) = clamp_slice(profile.len(), None, Some(split));
    let profile = &profile[..b];
    let k = 1.max((split as f64 * QUIET_SHARE) as i64) as usize;
    let mut out: Vec<i64> = argsort(profile).into_iter().take(k).map(|i| i as i64).collect();
    out.sort();
    out
}

/// Indices that sort `x` (NaN last); ties keep their order.
fn argsort<T: PartialOrd + Copy>(x: &[T]) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..x.len()).collect();
    idx.sort_by(|&a, &b| {
        let (va, vb) = (x[a], x[b]);
        let (na, nb) = (va.partial_cmp(&va).is_none(), vb.partial_cmp(&vb).is_none());
        match (na, nb) {
            (true, true) => std::cmp::Ordering::Equal,
            (true, false) => std::cmp::Ordering::Greater,
            (false, true) => std::cmp::Ordering::Less,
            _ => va.partial_cmp(&vb).unwrap_or(std::cmp::Ordering::Equal),
        }
    });
    idx
}

/// Noise power per frequency, measured in the call where the template's opening is quiet.
/// Uses the template's `quiet_frames`; an older template without them guesses the gaps from
/// the normalized pattern.
pub fn opening_noise(audio: &[f32], template: &Template, split: i64, offset: i64) -> Option<Vec<f32>> {
    match template.info.get("quiet_frames") {
        Some(q) if truthy(q) => {
            let quiet: Vec<i64> = q.as_array()?.iter().filter_map(value_int).collect();
            noise_at(audio, &quiet, offset)
        }
        _ => opening_noise_features(audio, template.features.view(), split, offset),
    }
}

/// `opening_noise` with only the template's features (the gaps guessed from them).
pub fn opening_noise_features(audio: &[f32], features: FView, split: i64, offset: i64) -> Option<Vec<f32>> {
    let opening = features.slice(None, Some(split));
    let sums: Vec<f32> = (0..opening.len())
        .map(|r| {
            let row = &opening.data[r * opening.cols..(r + 1) * opening.cols];
            sum_f32(row.len(), &|i| row[i])
        })
        .collect();
    let k = 1.max((opening.len() as f64 * QUIET_SHARE) as usize);
    let quiet: Vec<i64> = argsort(&sums).into_iter().take(k).map(|i| i as i64).collect();
    noise_at(audio, &quiet, offset)
}

fn noise_at(audio: &[f32], quiet: &[i64], offset: i64) -> Option<Vec<f32>> {
    let w = window();
    let mut fft = np::Fft32::new(WIENER_FFT);
    let mut frames: Vec<Vec<f32>> = Vec::new();
    for &i in quiet {
        let start = (offset + i) * HOP as i64;
        if start >= 0 && start as usize + WIENER_FFT <= audio.len() {
            let s = start as usize;
            let f: Vec<f32> = (0..WIENER_FFT).map(|j| audio[s + j] * w[j]).collect();
            frames.push(fft.rfft(&f).iter().map(|c| { let h = c.re.hypot(c.im); h * h }).collect());
        }
    }
    if frames.len() < 5 {
        return None;
    }
    let bins = WIENER_FFT / 2 + 1;
    let mut out = vec![0.0f32; bins];
    for f in &frames {
        for (o, v) in out.iter_mut().zip(f) {
            *o += v;
        }
    }
    let n = frames.len() as f32;
    Some(out.into_iter().map(|v| v / n).collect())
}

/// Wiener-style gain per frequency and moment, overlap-added back to audio.
pub fn remove_noise(audio: &[f32], noise: &[f32]) -> Vec<f32> {
    let n = audio.len();
    if n < WIENER_FFT {
        return audio.to_vec();
    }
    let bins = WIENER_FFT / 2 + 1;
    // one value applies to every frequency; any other length than one per frequency is refused
    let noise: Vec<f32> = match noise.len() {
        1 => vec![noise[0]; bins],
        l if l == bins => noise.to_vec(),
        l => {
            log::warn!("noise filter skipped: {l} noise values for {bins} frequencies");
            return audio.to_vec();
        }
    };
    let w = window();
    let mut padded = vec![0.0f32; n + 2 * WIENER_FFT];
    padded[WIENER_FFT..WIENER_FFT + n].copy_from_slice(audio);
    let count = (padded.len() - WIENER_FFT) / WIENER_HOP + 1;
    let mut fft = np::Fft32::new(WIENER_FFT);
    let mut out = vec![0.0f32; count * WIENER_HOP + WIENER_FFT];
    let mut norm = vec![0.0f32; out.len()];
    let mut frame = vec![0.0f32; WIENER_FFT];
    for k in 0..count {
        let s = k * WIENER_HOP;
        for j in 0..WIENER_FFT {
            frame[j] = padded[s + j] * w[j];
        }
        let spec: Vec<Complex<f32>> = fft
            .rfft(&frame)
            .into_iter()
            .zip(&noise)
            .map(|(c, &nz)| {
                let h = c.re.hypot(c.im);
                let p = h * h;
                let g = 1.0 - WIENER_OVER * nz / nan_max(p, 1e-12);
                c * nan_max(WIENER_FLOOR, g)
            })
            .collect();
        let back = fft.irfft(&spec);
        for j in 0..WIENER_FFT {
            out[s + j] += back[j] * w[j];
            norm[s + j] += w[j] * w[j];
        }
    }
    (WIENER_FFT..WIENER_FFT + n).map(|i| out[i] / nan_max(norm[i], 1e-6)).collect()
}

/// Voice band kept by `clean_voice`; radio voice carries nothing useful outside it.
pub const VOICE_LOW_HZ: f32 = 300.0;
pub const VOICE_HIGH_HZ: f32 = 3400.0;
/// Share of a call's frames taken as its noise (its quietest moments).
pub const CLEAN_QUIET_SHARE: f64 = 0.15;

/// Bring out the voice in a weak 16 kHz call before speech recognition. The noise is measured
/// in the call's own quietest moments and removed with the Wiener filter, and everything
/// outside the voice band is treated as noise too. On the first CSX detector recording this
/// turned "milepost sixteen point one" into "six, eight point one". The level is evened out
/// by the caller's normalization.
pub fn clean_voice(audio: &[f32]) -> Vec<f32> {
    if audio.len() < WIENER_FFT * 2 {
        return audio.to_vec();
    }
    let frames = (audio.len() - WIENER_FFT) / HOP + 1;
    let energy: Vec<f32> = (0..frames)
        .map(|i| audio[i * HOP..i * HOP + WIENER_FFT].iter().map(|v| v * v).sum())
        .collect();
    let k = 1.max((frames as f64 * CLEAN_QUIET_SHARE) as usize);
    let quiet: Vec<i64> = argsort(&energy).into_iter().take(k).map(|i| i as i64).collect();
    let Some(mut noise) = noise_at(audio, &quiet, 0) else { return audio.to_vec() };
    let bin_hz = 16_000.0 / WIENER_FFT as f32;
    let loudest = noise.iter().cloned().fold(0.0f32, f32::max).max(1e-12);
    for (b, nz) in noise.iter_mut().enumerate() {
        let f = b as f32 * bin_hz;
        if !(VOICE_LOW_HZ..=VOICE_HIGH_HZ).contains(&f) {
            // far above any frame's power there, so the gain drops to the floor
            *nz = loudest * 1e6;
        }
    }
    remove_noise(audio, &noise)
}

/// Larger of two values (NaN wins).
fn nan_max(a: f32, b: f32) -> f32 {
    if a.is_nan() || b.is_nan() { f32::NAN } else if a >= b { a } else { b }
}

/// Average several recordings of one announcement, each aligned to the first.
pub fn build<A: AsRef<[f32]>>(recordings: &[A]) -> Result<Feats, String> {
    let first = recordings.first().ok_or("no recordings")?;
    let reference = features(first.as_ref());
    let rv = reference.view();
    let r_len = rv.len() as i64;
    let mut stack: Vec<Feats> = vec![reference.clone()];
    for audio in &recordings[1..] {
        let f = features(audio.as_ref());
        let fv = f.view();
        let (mut best, mut shift) = (-2.0, 0);
        for off in -ALIGN_FRAMES..=ALIGN_FRAMES {
            let seg = fv.slice(Some(0.max(off)), None).slice(None, Some(r_len - 0.max(-off)));
            let r = rv.slice(Some(0.max(-off)), None).slice(None, Some(seg.len() as i64));
            let c = corr(seg.data, r.data);
            if c > best {
                best = c;
                shift = off;
            }
        }
        let mut aligned = vec![0.0f32; reference.data.len()];
        let seg = fv.slice(Some(0.max(shift)), None).slice(None, Some(r_len - 0.max(-shift)));
        let at = 0.max(-shift) as usize * reference.cols;
        let m = seg.data.len().min(aligned.len().saturating_sub(at));
        aligned[at..at + m].copy_from_slice(&seg.data[..m]);
        stack.push(Feats::new(aligned, reference.cols));
    }
    let k = stack.len() as f32;
    let mut sum = vec![0.0f32; reference.data.len()];
    for f in &stack {
        for (s, v) in sum.iter_mut().zip(&f.data) {
            *s += v;
        }
    }
    Ok(Feats::new(sum.into_iter().map(|v| v / k).collect(), reference.cols))
}

/// Length and rhythm, after `assess`. Changes the assessment in place.
///
/// Templates without opening_end_s: never an all-clear when longer than the template's.
/// (With opening_end_s, assess already refuses an ending followed by more message, because the
/// total length changes with the milepost number.) Both: an all-clear by rhythm and length when
/// the ending's detailed check just missed, for the template's own detector only (its number
/// heard), where the ending's place and the call's length are known.
pub fn check_length_and_rhythm(t: &Template, a: &mut Assessment, audio: &[f32], split: Option<i64>) {
    if a.is("all_clear") && a.method.as_deref().is_none_or(str::is_empty) {
        a.method = Some("sound".into());
    }
    let length = match t.info.get("length_s") {
        Some(v) if truthy(v) => v.as_f64().unwrap_or(0.0),
        _ => return,
    };
    let seconds = audio.len() as f64 / RATE as f64;
    a.length_s = Some(round_even(seconds, 2));
    let generic = t.info.get("opening_end_s").is_some_and(truthy);
    let split = split.filter(|&s| s != 0);
    if seconds > length + LONGER_MAX_S && !generic {
        if a.is("all_clear") {
            log::warn!("detector {}: the all-clear matched but the call is {:.1} s, longer than its \
                        {:.1} s; treated as a different message", t.name(), seconds, length);
        }
        a.kind = split.map(|_| "other".into());
        a.method = None;
        return;
    }
    let profile: Option<Vec<f64>> = match t.info.get("rhythm") {
        Some(Value::Array(p)) if !p.is_empty() => Some(p.iter().map(|v| v.as_f64().unwrap_or(f64::NAN)).collect()),
        _ => None,
    };
    if let (Some(profile), Some(split), Some(of)) = (profile, split, a.opening_frame) {
        if (a.is("other") || a.is("incomplete"))
            && a.milepost.as_deref() == Some("known")
            && length - SHORTER_MAX_S <= seconds
            && seconds <= length + LONGER_MAX_S
        {
            let expected = of + split;
            let r = rhythm_score(&profile, audio, split, expected);
            a.rhythm = Some(round_even(r, 2));
            let call = features(audio);
            let p = pieces(t.features.view().slice(Some(split), None), call.view(), Some(expected), ENDING_PIECES);
            let near = all_some_at_least(&p, RHYTHM_PIECE_MIN);
            if r >= RHYTHM_MIN && near {
                a.kind = Some("all_clear".into());
                a.method = Some("rhythm and length".into());
            }
        }
    }
}

// announcement text ------------------------------------------------------------------------------

fn strip_chars(s: &str, chars: &[char]) -> String {
    s.trim_matches(|c| chars.contains(&c)).to_string()
}

/// Trims whitespace at both ends, counting the separators U+001C to U+001F as whitespace too.
pub fn strip_space(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
}

fn re(pat: &'static str, cell: &'static OnceLock<Regex>) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pat).expect("regex"))
}

/// The identifying part of the announcement text: everything before "no defect".
pub fn opening_text(text: &str) -> String {
    static R: OnceLock<Regex> = OnceLock::new();
    match re(r"(?i)[,.]?\s*no defect", &R).find(text) {
        Some(m) => strip_chars(&text[..m.start()], &[' ', ',', '.']),
        None => strip_chars(text, &[' ', ',', '.']),
    }
}

/// The opening without its milepost number: "Norfolk Southern, milepost".
pub fn wording_text(text: &str) -> String {
    static R: OnceLock<Regex> = OnceLock::new();
    let o = opening_text(text);
    strip_chars(&re(r"\s*\d+(?:\.\d+)?\s*\z", &R).replace_all(&o, ""), &[' ', ',', '.'])
}

/// The all-clear's standard ending: "no defects. Repeat, no defects.".
pub fn ending_text(text: &str) -> String {
    static R: OnceLock<Regex> = OnceLock::new();
    match re(r"(?i)no defect", &R).find(text) {
        Some(m) => strip_space(&text[m.start()..]).to_string(),
        None => String::new(),
    }
}

// templates ----------------------------------------------------------------------------------------

/// Whether a JSON value counts as set: false for null, false, zero and empty strings, arrays
/// and objects.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn value_int(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_f64().map(|f| f.trunc() as i64))
}

/// A template: its metadata (the JSON file, exactly) and its features (the .npy file).
#[derive(Debug, Clone, PartialEq)]
pub struct Template {
    pub info: Map<String, Value>,
    pub features: Feats,
}

impl Template {
    pub fn name(&self) -> &str {
        self.info.get("name").and_then(Value::as_str).unwrap_or("")
    }
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.info.get(key)
    }
    pub fn split_frames(&self) -> Option<i64> {
        DetectorTemplates::split_frames(&self.info)
    }
    pub fn opening_end_frames(&self) -> Option<i64> {
        DetectorTemplates::opening_end_frames(&self.info)
    }
}

/// The templates in a directory, reloaded when the directory changes.
pub struct DetectorTemplates {
    pub dir: PathBuf,
    pub items: Vec<Template>,
    stamp: Option<SystemTime>,
}

fn seconds_to_frames(info: &Map<String, Value>, key: &str) -> Option<i64> {
    let s = info.get(key).filter(|v| truthy(v))?.as_f64()?;
    Some(round_int(s * RATE as f64 / HOP as f64))
}

/// Parses JSON, also taking the bare NaN, Infinity and -Infinity that template files may hold
/// for such floats (a rhythm value of a degenerate recording). They are read as null.
pub fn parse_json_lenient(text: &str) -> Result<Value, String> {
    match serde_json::from_str(text) {
        Ok(v) => Ok(v),
        Err(e) => {
            let fixed = non_finite_as_null(text);
            if fixed == text {
                return Err(e.to_string());
            }
            serde_json::from_str(&fixed).map_err(|_| e.to_string())
        }
    }
}

fn non_finite_as_null(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let (mut in_str, mut escaped) = (false, false);
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        if in_str {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
        } else if c == '"' {
            in_str = true;
        } else if let Some(word) = ["-Infinity", "Infinity", "NaN"].iter().find(|w| rest.starts_with(**w)) {
            out.push_str("null");
            rest = &rest[word.len()..];
            continue;
        }
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

fn read_template(meta: &Path) -> Result<Template, String> {
    let text = std::fs::read_to_string(meta).map_err(|e| e.to_string())?;
    let info = parse_json_lenient(&text)?;
    let Value::Object(info) = info else { return Err("not a JSON object".into()) };
    let arr = npy::load(&meta.with_extension("npy"))?;
    // a template of any other shape could never be compared with a call, so skip it
    if arr.shape.len() != 2 || arr.shape[1] != N_BANDS {
        return Err(format!("features of shape {:?}, not (frames, {N_BANDS})", arr.shape));
    }
    Ok(Template { info, features: Feats::new(arr.data, N_BANDS) })
}

impl DetectorTemplates {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        DetectorTemplates { dir: directory.into(), items: Vec::new(), stamp: None }
    }

    pub fn reload(&mut self) {
        let stamp = match std::fs::metadata(&self.dir).and_then(|m| m.modified()) {
            Ok(s) => s,
            Err(_) => {
                self.items.clear();
                self.stamp = None;
                return;
            }
        };
        if Some(stamp) == self.stamp {
            return;
        }
        let mut metas: Vec<PathBuf> = std::fs::read_dir(&self.dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| p.extension().is_some_and(|x| x == "json"))
                    .collect()
            })
            .unwrap_or_default();
        metas.sort();
        let mut items = Vec::new();
        for meta in metas {
            match read_template(&meta) {
                Ok(t) => items.push(t),
                Err(e) => log::warn!("detector template {} unreadable: {}",
                                     meta.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), e),
            }
        }
        self.items = items;
        self.stamp = Some(stamp);
        if !self.items.is_empty() {
            let names: Vec<&str> = self.items.iter().map(Template::name).collect();
            log::info!("detector templates: {}", names.join(", "));
        }
    }

    pub fn split_frames(t: &Map<String, Value>) -> Option<i64> {
        seconds_to_frames(t, "split_s")
    }

    pub fn opening_end_frames(t: &Map<String, Value>) -> Option<i64> {
        seconds_to_frames(t, "opening_end_s")
    }

    /// (template, assessment) for the best template whose detector is heard, or None.
    pub fn match_audio(&mut self, audio: &[f32]) -> Option<(Template, Assessment)> {
        self.reload();
        if self.items.is_empty() {
            return None;
        }
        let call = features(audio);
        let mut best: Option<(usize, Assessment)> = None;
        let key = |a: &Assessment| (a.is("all_clear"), a.milepost.as_deref() == Some("known"), a.score);
        for (i, t) in self.items.iter().enumerate() {
            let mut a = assess(t.features.view(), call.view(), t.split_frames(), t.opening_end_frames());
            if a.kind.is_some() {
                check_length_and_rhythm(t, &mut a, audio, t.split_frames());
                // an all-clear beats a bare opening match, a known milepost beats another; then the score
                let better = match &best {
                    None => true,
                    Some((_, b)) => {
                        let (x, y) = (key(&a), key(b));
                        (x.0, x.1).cmp(&(y.0, y.1)).then(x.2.partial_cmp(&y.2).unwrap_or(std::cmp::Ordering::Equal))
                            == std::cmp::Ordering::Greater
                    }
                };
                if better {
                    best = Some((i, a));
                }
            }
        }
        best.map(|(i, a)| (self.items[i].clone(), a))
    }

    // managing --------------------------------------------------------------------

    /// Make a template from recordings of one announcement; returns its metadata.
    pub fn add<A: AsRef<[f32]>>(&mut self, name: &str, text: &str, recordings: &[A], parsed: Option<Value>,
                                split_s: Option<f64>, opening_end_s: Option<f64>) -> Result<Map<String, Value>, String> {
        if !valid_name(name) {
            return Err("name: lowercase letters, digits, '.', '_' or '-'".into());
        }
        let feats = build(recordings)?;
        let seconds = (feats.len() * HOP) as f64 / RATE as f64;
        if let Some(s) = split_s {
            if !(1.0 <= s && s <= seconds - 1.0) {
                return Err(format!("split must be between 1 s and {:.1} s", seconds - 1.0));
            }
        }
        if let Some(o) = opening_end_s {
            let ok = matches!(split_s, Some(s) if s != 0.0 && 0.5 <= o && o <= s - 0.3);
            if !ok {
                return Err("opening end needs a split, and must be between 0.5 s and the split".into());
            }
        }
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        npy::save(&self.dir.join(format!("{name}.npy")), &[feats.len(), feats.cols], &feats.data)
            .map_err(|e| e.to_string())?;
        let opt = |v: Option<f64>| v.map(f64_value).unwrap_or(Value::Null);
        let mut info = Map::new();
        info.insert("name".into(), json!(name));
        info.insert("text".into(), json!(text));
        info.insert("opening_text".into(), json!(opening_text(text)));
        info.insert("parsed".into(), parsed.unwrap_or(Value::Null));
        info.insert("split_s".into(), opt(split_s));
        info.insert("opening_end_s".into(), opt(opening_end_s));
        info.insert("wording_text".into(), json!(wording_text(text)));
        info.insert("ending_text".into(), json!(ending_text(text)));
        info.insert("recordings".into(), json!(recordings.len()));
        info.insert("seconds".into(), f64_value(round_even(seconds, 1)));
        if let Some(s) = split_s.filter(|s| *s != 0.0) {
            info.insert("quiet_frames".into(), json!(quiet_frames(recordings, round_int(s * RATE as f64 / HOP as f64))));
        }
        let lengths: Vec<f64> = recordings.iter().map(|r| r.as_ref().len() as f64).collect();
        let length_s = round_even(np::median(&lengths) / RATE as f64, 2);
        info.insert("length_s".into(), f64_value(length_s));
        info.insert("all_clear_length_s".into(), f64_value(round_even(length_s - CLIP_LEAD_S, 2)));
        let rhythm: Vec<Value> = rhythm_profile(recordings).into_iter().map(|v| f64_value(round_even(v, 3))).collect();
        info.insert("rhythm".into(), Value::Array(rhythm));
        std::fs::write(self.dir.join(format!("{name}.json")), jsonfmt::dumps_indent(&Value::Object(info.clone()), 2) + "\n")
            .map_err(|e| e.to_string())?;
        self.stamp = None;
        Ok(info)
    }

    /// Each recording against a template made from the others (to check a new template).
    pub fn scores<A: AsRef<[f32]>>(&self, recordings: &[A], split_s: Option<f64>, opening_end_s: Option<f64>)
        -> Result<Vec<Assessment>, String> {
        let frames = |s: Option<f64>| s.filter(|v| *v != 0.0).map(|v| round_int(v * RATE as f64 / HOP as f64));
        let (split, o_end) = (frames(split_s), frames(opening_end_s));
        let mut out = Vec::new();
        for i in 0..recordings.len() {
            let rest: Vec<&[f32]> = recordings.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, r)| r.as_ref()).collect();
            let t = build(&rest)?;
            out.push(assess(t.view(), features(recordings[i].as_ref()).view(), split, o_end));
        }
        Ok(out)
    }

    pub fn remove(&mut self, name: &str) -> bool {
        let mut found = false;
        for suffix in [".json", ".npy"] {
            let p = self.dir.join(format!("{name}{suffix}"));
            if p.exists() && std::fs::remove_file(&p).is_ok() {
                found = true;
            }
        }
        self.stamp = None;
        found
    }

    /// Each template's metadata (without its features).
    pub fn list(&mut self) -> Vec<Map<String, Value>> {
        self.reload();
        self.items.iter().map(|t| t.info.clone()).collect()
    }
}

// API contract v1.5: the `detector` object on closed voice events ----------------------------------

// A voice clip starts this long before the event's start_ts (the SDR's pre-roll and detection
// delay): a 9.68 s clip for a 9.20 s transmission on the reference detector. Used to turn a template's clip
// length into the radio length the contract's rules are written in.
pub const CLIP_LEAD_S: f64 = 0.48;
pub const NOT_LONGER_S: f64 = 0.3; // v1.5: never no_defects when longer than the all-clear by more
pub const SHORTER_S: f64 = 1.0; // v1.5: incomplete when shorter than the all-clear by more
pub const SAME_DETECTOR_MI: f64 = 0.25; // v1.5 section 5: the headend's detector within this of the milepost
pub const RESULTS: [&str; 3] = ["no_defects", "different_message", "incomplete"];
pub const METHODS: [&str; 4] = ["sound", "rhythm_and_length", "opening", "words"]; // "words": contract v1.9

/// The template's all-clear as a radio length (s).
pub fn all_clear_length(t: &Map<String, Value>) -> Option<f64> {
    if let Some(v) = t.get("all_clear_length_s").filter(|v| truthy(v)) {
        return v.as_f64();
    }
    match t.get("length_s").filter(|v| truthy(v)) {
        Some(v) => v.as_f64().map(|l| round_even(l - CLIP_LEAD_S, 2)),
        None => None,
    }
}

/// (result, method) by v1.5 section 1, in its order.
pub fn contract_result(a: &Assessment, length_s: f64, all_clear_s: f64) -> (&'static str, &'static str) {
    if a.is("all_clear") && length_s <= all_clear_s + NOT_LONGER_S {
        let m = if a.method.as_deref() == Some("rhythm and length") { "rhythm_and_length" } else { "sound" };
        return ("no_defects", m);
    }
    if length_s < all_clear_s - SHORTER_S {
        return ("incomplete", "opening");
    }
    ("different_message", "opening")
}

fn score_value(x: Option<f64>) -> Value {
    match x {
        // clamp to [0, 1]; a NaN score fails both tests and so stays NaN
        Some(v) => {
            let v = if v > 0.0 { v } else { 0.0 };
            let v = if v < 1.0 { v } else { 1.0 };
            f64_value(round_even(v, 2))
        }
        None => Value::Null,
    }
}

/// The v1.5 `detector` object for a closed voice event.
#[allow(clippy::too_many_arguments)]
pub fn detector_object(detector_id: &str, railroad: &str, milepost: f64, result: &str, method: &str,
                       a: &Assessment, length_s: f64, all_clear_s: f64, repeat_of: Option<&str>,
                       heard_text: Option<&str>, timestamp: Option<f64>) -> Value {
    let ending: Vec<f64> = a.ending.iter().flatten().flatten().copied().collect();
    let ending_min = ending.iter().copied().reduce(|m, v| if v < m { v } else { m });
    let opening = if a.opening.as_ref().is_some_and(|o| !o.is_empty()) { score_value(Some(a.score)) } else { Value::Null };
    let heard = match heard_text {
        Some(h) if !h.is_empty() && result != "no_defects" => json!(h.chars().take(300).collect::<String>()),
        _ => Value::Null,
    };
    let mut scores = Map::new();
    scores.insert("opening".into(), opening);
    scores.insert("ending".into(), score_value(ending_min));
    scores.insert("rhythm".into(), score_value(a.rhythm));
    let mut obj = Map::new();
    obj.insert("detector_id".into(), json!(detector_id));
    obj.insert("railroad".into(), json!(railroad.to_uppercase()));
    obj.insert("milepost".into(), f64_value(round_even(milepost, 1)));
    obj.insert("result".into(), json!(result));
    obj.insert("method".into(), json!(method));
    obj.insert("scores".into(), Value::Object(scores));
    obj.insert("length_s".into(), f64_value(round_even(length_s, 2)));
    obj.insert("all_clear_length_s".into(), f64_value(round_even(all_clear_s, 2)));
    obj.insert("repeat_of".into(), json!(repeat_of));
    obj.insert("heard_text".into(), heard);
    if let Some(ts) = timestamp {
        obj.insert("timestamp".into(), f64_value(round_even(ts, 2)));
    }
    Value::Object(obj)
}

/// A JSON number (int or float) as f64; booleans and other types are not numbers.
fn real_number(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64)
}

/// What's wrong with a `detector` object by v1.5 section 1 (an invalid one would make the
/// headend refuse the whole batch). Empty when it's fine.
pub fn validate_detector(obj: &Value) -> Vec<String> {
    static UUID: OnceLock<Regex> = OnceLock::new();
    let uuid = re(r"^(?:[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})\z", &UUID);
    let empty = Map::new();
    let o = obj.as_object().unwrap_or(&empty);
    let mut errs: Vec<String> = Vec::new();
    let mut need = |cond: bool, msg: &str| {
        if !cond {
            errs.push(msg.to_string());
        }
    };
    let did = o.get("detector_id").and_then(Value::as_str);
    need(did.is_some_and(valid_name), "detector_id");
    let rr = o.get("railroad").and_then(Value::as_str);
    need(rr.is_some_and(|r| (1..=8).contains(&r.chars().count()) && r == r.to_uppercase()), "railroad");
    let mp = real_number(o.get("milepost"));
    need(mp.is_some_and(|m| (0.0..=10_000.0).contains(&m)), "milepost");
    let in_list = |k: &str, list: &[&str]| o.get(k).and_then(Value::as_str).is_some_and(|v| list.contains(&v));
    need(in_list("result", &RESULTS), "result");
    need(in_list("method", &METHODS), "method");
    let scores_ok = match o.get("scores") {
        Some(Value::Object(sc)) => {
            let mut keys: Vec<&str> = sc.keys().map(String::as_str).collect();
            keys.sort();
            keys == ["ending", "opening", "rhythm"]
                && sc.values().all(|v| match v {
                    Value::Null => true,
                    // booleans are accepted as scores, as 1 and 0
                    Value::Bool(_) => true,
                    Value::Number(n) => n.as_f64().is_some_and(|f| (0.0..=1.0).contains(&f)),
                    _ => false,
                })
        }
        _ => false,
    };
    need(scores_ok, "scores");
    for k in ["length_s", "all_clear_length_s"] {
        need(real_number(o.get(k)).is_some_and(|v| (0.0..=120.0).contains(&v)), k);
    }
    let ro = o.get("repeat_of");
    need(match ro {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => uuid.is_match(s),
        _ => false,
    }, "repeat_of");
    let ht = o.get("heard_text");
    let ht_none = matches!(ht, None | Some(Value::Null));
    need(ht_none || matches!(ht, Some(Value::String(s)) if s.chars().count() <= 300), "heard_text");
    need(!(o.get("result").and_then(Value::as_str) == Some("no_defects") && !ht_none), "heard_text on no_defects");
    if o.contains_key("timestamp") {
        need(real_number(o.get("timestamp")).is_some_and(|t| 1e9 < t && t < 1e10), "timestamp");
    }
    errs
}

/// A JSON value as text: strings bare, null as `None`, booleans as `True` and `False`, floats
/// as `float_repr` writes them.
fn value_str(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(b) => if *b { "True".into() } else { "False".into() },
        Value::String(s) => s.clone(),
        Value::Number(n) if n.is_f64() => jsonfmt::float_repr(n.as_f64().unwrap_or(f64::NAN)),
        other => other.to_string(),
    }
}

/// A JSON value as a float: numbers, booleans (1 and 0) and numeric strings (trimmed,
/// underscores allowed, inf and nan words); None for anything else.
pub fn value_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::String(s) => {
            let t = strip_space(s).replace('_', "");
            match t.to_ascii_lowercase().as_str() {
                "inf" | "+inf" | "infinity" | "+infinity" => Some(f64::INFINITY),
                "-inf" | "-infinity" => Some(f64::NEG_INFINITY),
                "nan" | "+nan" | "-nan" => Some(f64::NAN),
                _ => t.parse().ok(),
            }
        }
        _ => None,
    }
}

/// How a heard announcement was tied to a detector in the headend's list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectorMatch {
    /// the milepost heard is the listed one (within SAME_DETECTOR_MI)
    Heard,
    /// the milepost was misheard or not heard, but the listed detector is the likely one: it is
    /// the only one of that railroad in the list, or the closest by digits
    Suggested,
}

fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = (prev + usize::from(ca != cb)).min(row[j] + 1).min(row[j + 1] + 1);
            prev = cur;
        }
    }
    row[b.len()]
}

/// The listed detector a heard announcement most likely came from. A milepost within
/// SAME_DETECTOR_MI is a match. Otherwise, since a receiver hears few detectors of a railroad, a
/// misheard number ("sixteen point one" for 68.1) is suggested as the railroad's only listed
/// detector, or as the one whose digits differ least (at most 2 edits, and no tie).
pub fn suggest_detector<'a>(detectors: Option<&'a [Value]>, railroad: &str, heard: Option<f64>)
    -> Option<(&'a Value, DetectorMatch)> {
    if let Some(d) = headend_detector(detectors, railroad, heard) {
        return Some((d, DetectorMatch::Heard));
    }
    let same: Vec<&Value> = detectors.unwrap_or(&[]).iter()
        .filter(|d| d.get("railroad").map(value_str).unwrap_or_default().eq_ignore_ascii_case(railroad))
        .filter(|d| d.get("milepost").and_then(value_f64).is_some())
        .collect();
    match same.len() {
        0 => None,
        1 => Some((same[0], DetectorMatch::Suggested)),
        _ => {
            let heard = heard?;
            let digits = |m: f64| format!("{m:.1}").replace('.', "");
            let mut scored: Vec<(usize, &Value)> = same.iter()
                .map(|d| (edit_distance(&digits(heard), &digits(d.get("milepost").and_then(value_f64).unwrap_or(0.0))), *d))
                .collect();
            scored.sort_by_key(|(n, _)| *n);
            let best = scored[0].0;
            (best <= 2 && scored.get(1).is_none_or(|(n, _)| *n > best)).then(|| (scored[0].1, DetectorMatch::Suggested))
        }
    }
}

/// The headend's known detector (from /config) at this railroad and milepost, if any.
pub fn headend_detector<'a>(detectors: Option<&'a [Value]>, railroad: &str, milepost: Option<f64>) -> Option<&'a Value> {
    let detectors = detectors.filter(|d| !d.is_empty())?;
    let milepost = milepost?;
    for d in detectors {
        let Some(m) = d.as_object() else { continue };
        let rr = m.get("railroad").map(value_str).unwrap_or_default();
        let Some(mp) = m.get("milepost").and_then(value_f64) else { continue };
        if rr.to_uppercase() == railroad.to_uppercase() && (mp - milepost).abs() <= SAME_DETECTOR_MI {
            return Some(d);
        }
    }
    None
}
