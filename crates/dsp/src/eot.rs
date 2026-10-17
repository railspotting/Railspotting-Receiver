//! End-of-train (EOT) telemetry decoder: 1200 bps FFSK on NFM, BCH(63,45) with a key.
//!
//! Check constants come from an open-source EOT decoder (github.com/ereuter).

use num_complex::{Complex32, Complex64};
use serde_json::{json, Map, Value};
use std::f64::consts::PI;

use crate::bursts::lowpass;
use crate::round_even;

pub const DECODER: &str = "railspotting-eot-1";
pub const BAUD: f64 = 1200.0;
pub const MARK_HZ: f64 = 1200.0; // bit 1
pub const SPACE_HZ: f64 = 1800.0; // bit 0
pub const SYNC: [u8; 11] = [1, 1, 1, 0, 0, 0, 1, 0, 0, 1, 0];
pub const GENERATOR: [u8; 19] = [1, 1, 1, 1, 0, 0, 1, 1, 0, 1, 0, 0, 0, 0, 0, 1, 1, 1, 1];
pub const KEY: [u8; 18] = [1, 0, 1, 0, 1, 1, 0, 1, 1, 1, 0, 1, 1, 1, 0, 0, 0, 0];
pub const PAYLOAD_BITS: usize = 63;
pub const DATA_BITS: usize = 45;
pub const PREAMBLE_CHECK: usize = 16;
/// For a packet that needs two corrected bits: the bit changes needed in those 16 bits (of 15).
/// Weak bursts lose a few preamble bits next to the sync word; noise averages 7.5.
pub const PREAMBLE_LOOSE: usize = 9;

pub const FIELDS: [(usize, usize, &str); 6] = [ // (first bit, last bit, key)
    (0, 1, "chaining"),
    (2, 3, "battery_condition"),
    (4, 6, "message_type"),
    (7, 23, "unit_id"),
    (24, 30, "brake_pipe_psi"),
    (31, 37, "battery_charge_raw"),
];
pub const FLAGS: [(usize, &str); 6] = [
    (39, "valve_circuit_ok"),
    (40, "confirmation"),
    (41, "turbine"),
    (42, "motion"),
    (43, "marker_battery_weak"),
    (44, "marker_light"),
];

// check ------------------------------------------------------------------------

/// 18 check bits for 45 data bits: remainder of the reversed data, XOR the key.
pub fn check_bits(data45: &[u8]) -> Vec<u8> {
    let mut r: Vec<u8> = data45.iter().rev().map(|&b| b & 1).collect();
    r.extend([0u8; 18]);
    for i in 0..r.len() - 18 {
        if r[i] != 0 {
            for (j, g) in GENERATOR.iter().enumerate() {
                r[i + j] ^= g;
            }
        }
    }
    r[r.len() - 18..].iter().zip(KEY).map(|(a, b)| a ^ b).collect()
}

pub fn check_ok(p: &[u8]) -> bool {
    p.len() >= PAYLOAD_BITS && check_bits(&p[..DATA_BITS]) == p[DATA_BITS..PAYLOAD_BITS]
}

/// Try every single-bit flip. Never corrects more than one bit.
pub fn correct_one(p: &[u8]) -> Option<(Vec<u8>, usize)> {
    let p: Vec<u8> = p.iter().take(PAYLOAD_BITS).copied().collect();
    for i in 0..PAYLOAD_BITS.min(p.len()) {
        let mut q = p.clone();
        q[i] ^= 1;
        if check_ok(&q) {
            return Some((q, i));
        }
    }
    None
}

/// Try every pair of bit flips. The code corrects up to three errors, so a word within two flips
/// of a valid packet is within two of no other: the answer is unique. Still, two flips in noise
/// find a "packet" about once in 130 tries, so `decode` keeps one only when two timing offsets
/// agree on it.
pub fn correct_two(p: &[u8]) -> Option<Vec<u8>> {
    let p: Vec<u8> = p.iter().take(PAYLOAD_BITS).copied().collect();
    if p.len() < PAYLOAD_BITS {
        return None;
    }
    for i in 0..PAYLOAD_BITS {
        for j in i + 1..PAYLOAD_BITS {
            let mut q = p.clone();
            q[i] ^= 1;
            q[j] ^= 1;
            if check_ok(&q) {
                return Some(q);
            }
        }
    }
    None
}

/// The most corrected bits the headend accepts today (contract v1.2: 0 or 1). A headend that
/// takes more says so in `/config` as `eot_corrected_bits_max`.
pub const HEADEND_CORRECTED_MAX: i64 = 1;

/// The headend's limit on corrected bits, from its `/config`.
pub fn corrected_max(cfg: &Value) -> i64 {
    cfg.get("eot_corrected_bits_max").and_then(Value::as_i64).unwrap_or(HEADEND_CORRECTED_MAX)
}

/// Keep telemetry the headend would refuse off an event: it goes as an undecoded burst, as it
/// did before two-bit corrections. True when telemetry was taken off.
pub fn fit_for_headend(event: &mut Map<String, Value>, max_corrected: i64) -> bool {
    let over = event.get("telemetry")
        .and_then(|t| t.get("corrected_bits"))
        .and_then(Value::as_i64)
        .is_some_and(|c| c > max_corrected);
    if over {
        event.remove("telemetry");
    }
    over
}

// fields -----------------------------------------------------------------------

fn value(p: &[u8], first: usize, last: usize) -> i64 {
    (0..=last - first).map(|i| (p[first + i] as i64) << i).sum()
}

pub fn fields(p: &[u8], corrected_bits: i64) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert("protocol".into(), json!("eot"));
    for (first, last, key) in FIELDS {
        out.insert(key.into(), json!(value(p, first, last)));
    }
    for (bit, key) in FLAGS {
        out.insert(key.into(), json!(p[bit] != 0));
    }
    out.insert("decoder".into(), json!(DECODER));
    out.insert("crc_ok".into(), json!(true));
    out.insert("corrected_bits".into(), json!(corrected_bits));
    out
}

/// What goes into a payload; `Payload::new` fills in the usual defaults.
#[derive(Debug, Clone)]
pub struct Payload {
    pub unit_id: i64,
    pub brake_pipe_psi: i64,
    pub motion: bool,
    pub marker_light: bool,
    pub chaining: i64,
    pub battery_condition: i64,
    pub message_type: i64,
    pub battery_charge_raw: i64,
    pub valve_circuit_ok: bool,
    pub confirmation: bool,
    pub turbine: bool,
    pub marker_battery_weak: bool,
}

impl Payload {
    pub fn new(unit_id: i64, brake_pipe_psi: i64) -> Self {
        Payload { unit_id, brake_pipe_psi, motion: true, marker_light: false, chaining: 3, battery_condition: 3,
                  message_type: 0, battery_charge_raw: 0, valve_circuit_ok: true, confirmation: true,
                  turbine: true, marker_battery_weak: false }
    }

    /// Assemble a 63-bit payload with a valid check (for the simulator and tests).
    pub fn bits(&self) -> Vec<u8> {
        let vals = |k: &str| match k {
            "chaining" => self.chaining, "battery_condition" => self.battery_condition,
            "message_type" => self.message_type, "unit_id" => self.unit_id,
            "brake_pipe_psi" => self.brake_pipe_psi, _ => self.battery_charge_raw,
        };
        let flags = |k: &str| match k {
            "valve_circuit_ok" => self.valve_circuit_ok, "confirmation" => self.confirmation,
            "turbine" => self.turbine, "motion" => self.motion,
            "marker_battery_weak" => self.marker_battery_weak, _ => self.marker_light,
        };
        let mut d = vec![0u8; DATA_BITS];
        for (first, last, key) in FIELDS {
            for i in 0..=last - first {
                d[first + i] = ((vals(key) >> i) & 1) as u8;
            }
        }
        for (bit, key) in FLAGS {
            d[bit] = flags(key) as u8;
        }
        let c = check_bits(&d);
        d.extend(c);
        d
    }
}

/// `build_payload(unit_id, brake_pipe_psi)` with every other field at its default.
pub fn build_payload(unit_id: i64, brake_pipe_psi: i64) -> Vec<u8> {
    Payload::new(unit_id, brake_pipe_psi).bits()
}

pub fn packet_bits(payload: &[u8]) -> Vec<u8> {
    packet_bits_with(payload, 69)
}

pub fn packet_bits_with(payload: &[u8], preamble_bits: usize) -> Vec<u8> {
    let mut out: Vec<u8> = (0..preamble_bits).map(|i| (i % 2) as u8).collect(); // 0101...
    out.extend(SYNC);
    out.extend_from_slice(payload);
    out
}

/// Continuous-phase FFSK tones: 1200 Hz for 1, 1800 Hz for 0.
pub fn ffsk_audio(bits: &[u8], fs: f64) -> Vec<f64> {
    let spb = fs / BAUD;
    let n = (bits.len() as f64 * spb).round_ties_even() as usize;
    let mut acc = 0.0f64;
    (0..n).map(|k| {
        let idx = ((k as f64 / spb) as usize).min(bits.len() - 1);
        acc += if bits[idx] == 1 { MARK_HZ } else { SPACE_HZ };
        (2.0 * PI * acc / fs).sin()
    }).collect()
}

pub fn fm_modulate(audio: &[f64], fs: f64, deviation_hz: f64, offset_hz: f64) -> Vec<Complex64> {
    let mut acc = 0.0f64;
    audio.iter().enumerate().map(|(k, a)| {
        acc += a;
        let t = k as f64 / fs;
        let phase = 2.0 * PI * deviation_hz * acc / fs + 2.0 * PI * offset_hz * t;
        Complex64::new(phase.cos(), phase.sin())
    }).collect()
}

// demodulation -----------------------------------------------------------------

/// Convolution of `x` with a real `h`, cut to the centre `max(len(x), len(h))` samples
/// ("same" mode), for a long `x`.
fn convolve_same(x: &[Complex64], h: &[f64]) -> Vec<Complex64> {
    let (n, m) = (x.len() as isize, h.len() as isize);
    let off = (m - 1) / 2;
    let len = n.max(m);
    (0..len).map(|i| {
        let k = i + off; // index into the full convolution
        let j0 = (k - n + 1).max(0);
        let j1 = k.min(m - 1);
        let mut acc = Complex64::new(0.0, 0.0);
        for j in j0..=j1 {
            acc += x[(k - j) as usize] * h[j as usize];
        }
        acc
    }).collect()
}

pub fn median(v: &[f64]) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.total_cmp(b));
    let n = s.len();
    if n % 2 == 1 { s[n / 2] } else { (s[n / 2 - 1] + s[n / 2]) / 2.0 }
}

/// Soft FFSK stream at the input rate: > 0 means 1.
pub fn soft_bits(iq: &[Complex64], fs: f64) -> Vec<f64> {
    let taps = lowpass(129, 7_000.0, fs);
    let y = convolve_same(iq, &taps);
    let mut disc: Vec<f64> = y.windows(2).map(|w| (w[1] * w[0].conj()).arg() * fs / (2.0 * PI)).collect();
    let med = median(&disc);
    for d in &mut disc {
        *d -= med; // remove the dongle's tuning error
    }
    let spb = (fs / BAUD).round_ties_even() as usize;
    let bx = vec![1.0 / spb as f64; spb];
    let mix = |f: f64| -> Vec<Complex64> {
        disc.iter().enumerate().map(|(k, &d)| {
            let a = -2.0 * PI * f * (k as f64 / fs);
            Complex64::new(a.cos(), a.sin()) * d
        }).collect()
    };
    let env1 = convolve_same(&mix(MARK_HZ), &bx);
    let env0 = convolve_same(&mix(SPACE_HZ), &bx);
    env1.iter().zip(env0).map(|(a, b)| a.norm() - b.norm()).collect()
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from > hay.len() || needle.len() > hay.len() {
        return None;
    }
    (from..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

fn clean_preamble(bits: &[u8], sync_start: usize) -> bool {
    if sync_start < PREAMBLE_CHECK {
        return false;
    }
    let pre = &bits[sync_start - PREAMBLE_CHECK..sync_start];
    pre.windows(2).all(|w| w[0] != w[1])
}

/// A preamble that is mostly there: enough bit changes before the sync word.
fn loose_preamble(bits: &[u8], sync_start: usize) -> bool {
    if sync_start < PREAMBLE_CHECK {
        return false;
    }
    let pre = &bits[sync_start - PREAMBLE_CHECK..sync_start];
    pre.windows(2).filter(|w| w[0] != w[1]).count() >= PREAMBLE_LOOSE
}

/// Every valid packet in a burst, in time order.
///
/// Each of eight timing offsets reads the bits. A packet that passes its check, or passes after
/// one flipped bit behind a clean preamble, is kept. One that needs two flips, behind a preamble
/// that is mostly there, is kept only when two offsets read the same packet at the same place.
pub fn decode(iq: &[Complex64], fs: f64) -> Vec<Map<String, Value>> {
    let spb_f = fs / BAUD;
    if (iq.len() as f64) < spb_f * (SYNC.len() + PAYLOAD_BITS) as f64 {
        return vec![];
    }
    let soft = soft_bits(iq, fs);
    // (sample position, corrected bits, payload)
    let mut found: Vec<(i64, i64, Vec<u8>)> = Vec::new();
    // two-flip candidates: (sample position, offset, payload)
    let mut two: Vec<(i64, usize, Vec<u8>)> = Vec::new();
    for k in 0..8 {
        let offset = k as f64 * spb_f / 8.0;
        let count = ((soft.len() as f64 - offset) / spb_f) as usize;
        let idx: Vec<usize> = (0..count).map(|i| (offset + i as f64 * spb_f) as usize).collect();
        let bits: Vec<u8> = idx.iter().map(|&i| (soft[i] > 0.0) as u8).collect();
        let mut start = find(&bits, &SYNC, 0);
        while let Some(s) = start {
            let p0 = s + SYNC.len();
            if p0 + PAYLOAD_BITS <= bits.len() {
                let p = bits[p0..p0 + PAYLOAD_BITS].to_vec();
                let pos = idx[s] as i64;
                if check_ok(&p) {
                    found.push((pos, 0, p));
                } else if let Some(fixed) = clean_preamble(&bits, s).then(|| correct_one(&p)).flatten() {
                    found.push((pos, 1, fixed.0));
                } else if loose_preamble(&bits, s) {
                    if let Some(fixed) = correct_one(&p).is_none().then(|| correct_two(&p)).flatten() {
                        two.push((pos, k, fixed));
                    }
                }
            }
            start = find(&bits, &SYNC, s + 1);
        }
    }
    for (i, (pos, k, p)) in two.iter().enumerate() {
        let agree = two.iter().enumerate().any(|(j, (pos2, k2, p2))| {
            j != i && k2 != k && p2 == p && ((pos2 - pos).abs() as f64) <= spb_f
        });
        if agree {
            found.push((*pos, 2, p.clone()));
        }
    }
    // one packet per position (within 40 bits); fewer corrections win
    found.sort_by_key(|f| (f.1, f.0));
    let mut kept: Vec<(i64, i64, Vec<u8>)> = Vec::new();
    for f in found {
        if kept.iter().all(|k| ((f.0 - k.0).abs() as f64) > 40.0 * spb_f) {
            kept.push(f);
        }
    }
    kept.sort_by_key(|f| f.0);
    kept.into_iter().map(|(pos, corrected, p)| {
        let mut pkt = fields(&p, corrected);
        pkt.insert("t_offset_s".into(), json!(round_even(pos as f64 / fs, 4)));
        pkt
    }).collect()
}

/// `decode` for float32 samples (the burst recorder's and the .cf32 files' format).
pub fn decode32(iq: &[Complex32], fs: f64) -> Vec<Map<String, Value>> {
    let v: Vec<Complex64> = iq.iter().map(|c| Complex64::new(c.re as f64, c.im as f64)).collect();
    decode(&v, fs)
}

/// The newest packet plus the packet count, as sent on a closed EOT event. A packet that needed
/// two corrected bits is used only when the burst has no better one.
pub fn telemetry_from(packets: &[Map<String, Value>]) -> Option<Map<String, Value>> {
    let corrected = |p: &Map<String, Value>| p.get("corrected_bits").and_then(Value::as_i64).unwrap_or(0);
    let last = packets.iter().rev().find(|p| corrected(p) <= HEADEND_CORRECTED_MAX).or(packets.last())?;
    let mut tel: Map<String, Value> = last.iter().filter(|(k, _)| *k != "t_offset_s")
        .map(|(k, v)| (k.clone(), v.clone())).collect();
    tel.insert("packets".into(), json!(packets.len()));
    Some(tel)
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|x| x != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

pub fn describe(tel: &Map<String, Value>) -> String {
    let show = |k: &str| tel.get(k).map(|v| v.to_string()).unwrap_or_else(|| "None".into());
    let mut text = format!("EOT unit {}, {} psi, {}", show("unit_id"), show("brake_pipe_psi"),
                           if truthy(tel.get("motion")) { "moving" } else { "stopped" });
    match tel.get("corrected_bits").and_then(Value::as_i64).unwrap_or(0) {
        0 => {}
        1 => text += " (1 bit corrected)",
        n => text += &format!(" ({n} bits corrected)"),
    }
    text
}
