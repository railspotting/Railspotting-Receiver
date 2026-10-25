//! ITC (Interoperable Train Control, 220 MHz PTC) signal processing: the channel plan, a
//! channelizer, burst detection and pi/4-DQPSK demodulation. Receive only.
//!
//! What is known publicly: channels are 25 kHz wide on the grid 217.5875 MHz + n x 25 kHz,
//! the band in use runs 220.1125 to 221.9875 MHz, and the modulation is pi/4-DQPSK at 8 or 16
//! kilosymbols/s in short bursts. Framing and coding are worked out from recordings.

use num_complex::Complex32;
use serde::Serialize;
use std::collections::VecDeque;
use std::f32::consts::PI;

pub const GRID0_HZ: f64 = 217.5875e6;
pub const STEP_HZ: f64 = 25e3;
pub const BAND_LO_HZ: f64 = 220.1125e6;
pub const BAND_HI_HZ: f64 = 221.9875e6;
pub const SYMBOL_RATES: [f64; 2] = [16_000.0, 8_000.0];

/// The grid channel number of a frequency (217.5875 MHz is 0).
pub fn channel_number(hz: f64) -> i64 {
    ((hz - GRID0_HZ) / STEP_HZ).round() as i64
}

pub fn channel_hz(n: i64) -> f64 {
    GRID0_HZ + n as f64 * STEP_HZ
}

/// The in-use channels a capture at `center_hz` and `rate` can hear: inside the usable part of
/// the band (90% of the span) and clear of the DC spike at the centre.
pub fn channels_in(center_hz: f64, rate: f64) -> Vec<f64> {
    let half = 0.45 * rate - 15e3;
    (channel_number(BAND_LO_HZ)..=channel_number(BAND_HI_HZ))
        .map(channel_hz)
        .filter(|f| (f - center_hz).abs() <= half && (f - center_hz).abs() >= 15e3)
        .collect()
}

/// The decimation that brings `rate` to a channel rate of at least 96 kS/s that is a whole
/// multiple of 16 kS/s (so both symbol rates get whole samples per symbol).
pub fn decimation(rate: f64) -> Option<usize> {
    (1..=(rate / 96_000.0) as usize).rev().find(|d| {
        let out = rate / *d as f64;
        (out / 16_000.0 - (out / 16_000.0).round()).abs() < 1e-9
    })
}

/// Sample rates a capture may use, smallest (least work) first: each gives whole samples per
/// symbol after `decimation`, and the RTL-SDR runs at all of them.
pub const CAPTURE_RATES: [f64; 3] = [1.024e6, 2.048e6, 2.4e6];

/// A capture (centre, sample rate) that hears every one of `channels` (grid numbers): the
/// smallest rate that spans them, centred between them and moved off the DC spike if a channel
/// would sit on it. None if they are too far apart for one dongle.
pub fn plan_capture(channels: &[i64]) -> Option<(f64, f64)> {
    let lo = channel_hz(*channels.iter().min()?);
    let hi = channel_hz(*channels.iter().max()?);
    for &rate in &CAPTURE_RATES {
        let mid = (lo + hi) / 2.0;
        // centres between grid channels first, so no channel lands on DC
        for k in 0..=40 {
            for sign in [1.0, -1.0] {
                let center = mid + STEP_HZ / 2.0 + sign * k as f64 * STEP_HZ / 2.0;
                let heard = channels_in(center, rate);
                if channels.iter().all(|&c| heard.iter().any(|&f| channel_number(f) == c)) {
                    return Some(((center / 1e3).round() * 1e3, rate));
                }
            }
        }
    }
    None
}

/// A windowed-sinc low-pass filter.
pub fn lowpass(taps: usize, cutoff_hz: f64, rate: f64) -> Vec<f32> {
    let m = (taps - 1) as f64;
    let fc = cutoff_hz / rate;
    let h: Vec<f64> = (0..taps)
        .map(|i| {
            let x = i as f64 - m / 2.0;
            let sinc = if x == 0.0 { 2.0 * fc } else { (2.0 * std::f64::consts::PI * fc * x).sin() / (std::f64::consts::PI * x) };
            let w = 0.42 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / m).cos() + 0.08 * (4.0 * std::f64::consts::PI * i as f64 / m).cos();
            sinc * w
        })
        .collect();
    let sum: f64 = h.iter().sum();
    h.iter().map(|v| (v / sum) as f32).collect()
}

/// One channel taken out of a wide capture: mixed to 0 Hz, filtered and decimated.
pub struct Channel {
    pub hz: f64,
    pub out_rate: f64,
    step: Complex32,
    phase: Complex32,
    taps: Vec<f32>,
    hist: VecDeque<Complex32>,
    decim: usize,
    count: usize,
    renorm: usize,
}

impl Channel {
    pub fn new(hz: f64, center_hz: f64, rate: f64, decim: usize) -> Channel {
        let w = (-2.0 * std::f64::consts::PI * (hz - center_hz) / rate) as f32;
        let taps = lowpass(16 * decim + 1, 12_500.0, rate);
        Channel {
            hz,
            out_rate: rate / decim as f64,
            step: Complex32::from_polar(1.0, w),
            phase: Complex32::new(1.0, 0.0),
            hist: VecDeque::from(vec![Complex32::new(0.0, 0.0); taps.len()]),
            taps,
            decim,
            count: 0,
            renorm: 0,
        }
    }

    /// Feed wide-band samples; appends this channel's samples to `out`.
    pub fn process(&mut self, input: &[Complex32], out: &mut Vec<Complex32>) {
        for &x in input {
            self.hist.pop_front();
            self.hist.push_back(x * self.phase);
            self.phase *= self.step;
            self.renorm += 1;
            if self.renorm == 1024 {
                self.phase /= self.phase.norm();
                self.renorm = 0;
            }
            self.count += 1;
            if self.count == self.decim {
                self.count = 0;
                let (a, b) = self.hist.as_slices();
                let mut acc = Complex32::new(0.0, 0.0);
                for (h, s) in self.taps.iter().zip(a.iter().chain(b.iter())) {
                    acc += s * *h;
                }
                out.push(acc);
            }
        }
    }
}

/// A transmission found on a channel, with its samples at the channel rate.
#[derive(Debug, Clone)]
pub struct Burst {
    pub channel_hz: f64,
    pub rate: f64,
    /// Unix seconds of the first sample (including the lead-in).
    pub start: f64,
    pub duration_s: f64,
    /// Highest level above the channel's noise floor, dB.
    pub peak_db: f32,
    /// The noise floor itself, dB relative to full scale.
    pub floor_db: f32,
    pub samples: Vec<Complex32>,
}

/// Finds bursts on one channel: level above a running noise floor, with hysteresis.
pub struct BurstFinder {
    hz: f64,
    rate: f64,
    win: usize,
    open_db: f32,
    close_db: f32,
    levels: VecDeque<f32>,
    floor: Option<f32>,
    since_floor: usize,
    lead: VecDeque<Complex32>,
    cur: Option<(Vec<Complex32>, f32, usize, f64)>,
    acc: Vec<Complex32>,
    n: u64,
    start: f64,
}

impl BurstFinder {
    /// `start` is the Unix time of the stream's first sample.
    pub fn new(hz: f64, rate: f64, start: f64, open_db: f32) -> BurstFinder {
        BurstFinder {
            hz,
            rate,
            win: (rate * 0.002) as usize,
            open_db,
            close_db: open_db - 3.0,
            levels: VecDeque::new(),
            floor: None,
            since_floor: 0,
            lead: VecDeque::new(),
            cur: None,
            acc: vec![],
            n: 0,
            start,
        }
    }

    pub fn floor_db(&self) -> Option<f32> {
        self.floor
    }

    /// The channel this finder watches, Hz.
    pub fn hz(&self) -> f64 {
        self.hz
    }

    /// Feed channel samples; finished bursts are appended to `out`.
    pub fn process(&mut self, input: &[Complex32], out: &mut Vec<Burst>) {
        for &x in input {
            self.acc.push(x);
            if self.acc.len() < self.win {
                continue;
            }
            let block = std::mem::take(&mut self.acc);
            let p = block.iter().map(|c| c.norm_sqr()).sum::<f32>() / block.len() as f32;
            let db = 10.0 * (p + 1e-20).log10();
            let t = self.start + self.n as f64 / self.rate;
            self.n += block.len() as u64;
            self.levels.push_back(db);
            if self.levels.len() > 5000 {
                self.levels.pop_front(); // 10 s of history
            }
            self.since_floor += 1;
            if self.floor.is_none() && self.levels.len() >= 250 || self.since_floor >= 250 {
                let mut v: Vec<f32> = self.levels.iter().copied().collect();
                v.sort_by(|a, b| a.total_cmp(b));
                self.floor = Some(v[v.len() / 2]);
                self.since_floor = 0;
            }
            let Some(floor) = self.floor else {
                self.keep_lead(&block);
                continue;
            };
            match &mut self.cur {
                Some((samples, peak, quiet, _)) => {
                    samples.extend_from_slice(&block);
                    *peak = peak.max(db - floor);
                    if db - floor < self.close_db {
                        *quiet += 1;
                    } else {
                        *quiet = 0;
                    }
                    // closed after 6 ms below, or cut at 2 s (longer is not ITC data)
                    if *quiet >= 3 || samples.len() as f64 > 2.0 * self.rate {
                        let Some((samples, peak, _, start)) = self.cur.take() else { continue };
                        out.push(Burst {
                            channel_hz: self.hz,
                            rate: self.rate,
                            start,
                            duration_s: samples.len() as f64 / self.rate,
                            peak_db: peak,
                            floor_db: floor,
                            samples,
                        });
                    }
                }
                None if db - floor >= self.open_db => {
                    let mut samples: Vec<Complex32> = self.lead.drain(..).collect();
                    let start = t - samples.len() as f64 / self.rate;
                    samples.extend_from_slice(&block);
                    self.cur = Some((samples, db - floor, 0, start));
                }
                None => self.keep_lead(&block),
            }
        }
    }

    /// 4 ms before an opening are kept, so a burst's start (its preamble) isn't cut.
    fn keep_lead(&mut self, block: &[Complex32]) {
        self.lead.extend(block.iter().copied());
        while self.lead.len() > 2 * self.win {
            self.lead.pop_front();
        }
    }
}

/// With the channel's noise known, the demodulator ignores samples below this many times its RMS
/// amplitude (about 6 dB above it; noise passes it about 2% of the time)...
const NOISE_GATE: f32 = 2.0;
/// ...and below this share of the burst's peak.
const PEAK_GATE: f32 = 0.05;
/// A burst is split into transmissions where the signal drops out for more symbols than this...
const SEGMENT_GAP_SYMBOLS: usize = 3;
/// ...keeping stretches at least this long (3 ms at 16 ksym/s; the shortest frame is longer).
const SEGMENT_MIN_SYMBOLS: usize = 48;

/// A burst demodulated as pi/4-DQPSK.
#[derive(Debug, Clone, Serialize)]
pub struct Demod {
    /// The sample in the burst where the first symbol was taken (a burst can hold several
    /// transmissions, each demodulated on its own); symbol k is at `first + k * sps`.
    pub first: usize,
    pub symbol_rate: f64,
    /// Samples per symbol and the chosen sampling phase within a symbol.
    pub sps: usize,
    pub offset: usize,
    /// How well the phase changes sit on +/-45 and +/-135 degrees once the frequency offset is
    /// taken out, 0 (noise) to 1 (perfect).
    pub quality: f32,
    pub freq_offset_hz: f32,
    /// One digit per symbol, the phase change: 0 = +45, 1 = +135, 2 = -135, 3 = -45 degrees.
    pub symbols: String,
    /// How sure each symbol is, 0 to 255: how far its phase change sat from a decision boundary
    /// (255 is right on one of the four ideal changes; a gap is 0).
    #[serde(skip)]
    pub conf: Vec<u8>,
}

/// Demodulate one burst: try both symbol rates and every sampling phase, keep the one whose
/// phase changes best fit pi/4-DQPSK, correct the frequency offset and slice the symbols.
pub fn demod(samples: &[Complex32], rate: f64) -> Option<Demod> {
    demod_phases(samples, rate).into_iter().next()
}

/// Like `demod`, but one result for every sampling phase at the chosen symbol rate, the best
/// first. Frames in a long burst can sit best at different phases, so a decoder can try each.
pub fn demod_phases(samples: &[Complex32], rate: f64) -> Vec<Demod> {
    demod_gated(samples, rate, None)
}

/// `demod` for a burst from `BurstFinder`, gated against the channel's noise floor.
pub fn demod_burst(b: &Burst) -> Option<Demod> {
    demod_burst_phases(b).into_iter().next()
}

/// `demod_phases` for a burst from `BurstFinder`, gated against the channel's noise floor.
pub fn demod_burst_phases(b: &Burst) -> Vec<Demod> {
    demod_gated(&b.samples, b.rate, Some(10f32.powf(b.floor_db / 20.0)))
}

/// The demodulator, ignoring samples too weak to be signal. With the channel's noise (RMS
/// amplitude) known, the gate is a little above it: a long burst can hold frames from several
/// wayside units at different levels, and a gate at a share of the strongest would silence the
/// weaker ones. Without it, a share of the burst's peak.
fn demod_gated(samples: &[Complex32], rate: f64, noise_rms: Option<f32>) -> Vec<Demod> {
    let peak = samples.iter().map(|c| c.norm()).fold(0.0f32, f32::max);
    if peak == 0.0 {
        return vec![];
    }
    let strong = match noise_rms {
        Some(n) => (NOISE_GATE * n).max(PEAK_GATE * peak),
        None => 0.3 * peak,
    };
    // A long burst is often several transmissions back to back (TDMA slots from different
    // wayside units), each with its own rate, timing and frequency error: when there are gaps
    // between them, each is demodulated on its own too. The whole burst stays first (a frame can
    // dip at its own edges and be cut by the split); the decoder keeps each frame once.
    let mut out = demod_stretch(samples, rate, strong, 0);
    let segs = segments(samples, rate, strong);
    if segs.len() > 1 {
        out.extend(segs.into_iter().flat_map(|(a, b)| demod_stretch(&samples[a..b], rate, strong, a)));
    }
    out
}

/// Stretches of signal in a burst: runs of symbol-long blocks above the gate, split where the
/// signal drops out for more than a few symbols, and long enough to hold a frame.
fn segments(samples: &[Complex32], rate: f64, strong: f32) -> Vec<(usize, usize)> {
    let blk = (rate / SYMBOL_RATES[0]).round().max(1.0) as usize;
    let on: Vec<bool> = samples.chunks(blk).map(|c| c.iter().map(|x| x.norm()).sum::<f32>() / c.len() as f32 > strong).collect();
    let mut out = vec![];
    let mut i = 0;
    while i < on.len() {
        if !on[i] {
            i += 1;
            continue;
        }
        let first = i;
        let mut last = i;
        let mut gap = 0;
        i += 1;
        while i < on.len() && gap <= SEGMENT_GAP_SYMBOLS {
            if on[i] {
                last = i;
                gap = 0;
            } else {
                gap += 1;
            }
            i += 1;
        }
        if last + 1 - first >= SEGMENT_MIN_SYMBOLS {
            // a symbol either side, for the differential phase at the edges
            out.push((first.saturating_sub(2) * blk, ((last + 3) * blk).min(samples.len())));
        }
    }
    out
}

/// Demodulate one stretch of a burst (starting at `start` in it): try both symbol rates and
/// every sampling phase, keep the one whose phase changes best fit pi/4-DQPSK, correct the
/// frequency offset and slice the symbols.
fn demod_stretch(samples: &[Complex32], rate: f64, strong: f32, start: usize) -> Vec<Demod> {
    let mut best: Option<(f32, f64, usize, usize, Complex32)> = None;
    for &sr in &SYMBOL_RATES {
        let sps = (rate / sr).round() as usize;
        if sps < 2 || ((rate / sr) - sps as f64).abs() > 1e-6 {
            continue;
        }
        for off in 0..sps {
            let mut acc = Complex32::new(0.0, 0.0);
            let mut count = 0;
            let mut i = off + sps;
            while i < samples.len() {
                let (a, b) = (samples[i], samples[i - sps]);
                if a.norm() > strong && b.norm() > strong {
                    let d = a * b.conj();
                    let u = d / d.norm();
                    acc += u * u * u * u;
                    count += 1;
                }
                i += sps;
            }
            if count < 16 {
                continue;
            }
            // pi/4-DQPSK phase changes are odd multiples of 45 degrees, so their 4th power is -1,
            // turned by four times the frequency offset's rotation per symbol. The size says how
            // well it fits. A wrong rate also fits (its changes are multiples of 90 degrees) but
            // then implies an offset near sr/8, far beyond a dongle's error: those are refused.
            let m = -acc / count as f32;
            let offset_hz = m.arg() / 4.0 * sr as f32 / (2.0 * PI);
            let q = if offset_hz.abs() < sr as f32 / 16.0 { m.norm() } else { m.norm() - 1.0 };
            if best.as_ref().is_none_or(|b| q > b.0) {
                best = Some((q, sr, sps, off, m));
            }
        }
    }
    let Some((quality, sr, sps, best_off, m)) = best else { return vec![] };
    // the leftover rotation per symbol is the frequency offset
    let rot = m.arg() / 4.0;
    let freq_offset_hz = rot * sr as f32 / (2.0 * PI);
    let derot = Complex32::from_polar(1.0, -rot);
    let order = std::iter::once(best_off).chain((0..sps).filter(|&o| o != best_off));
    order
        .map(|off| {
            let (symbols, conf, first) = slice(samples, sps, off, strong, derot);
            Demod { first: start + first, symbol_rate: sr, sps, offset: off, quality, freq_offset_hz, symbols, conf }
        })
        .collect()
}

/// Phase changes at one sampling phase to symbol digits, how sure each is, and the sample of the
/// first one.
fn slice(samples: &[Complex32], sps: usize, off: usize, strong: f32, derot: Complex32) -> (String, Vec<u8>, usize) {
    let mut symbols = String::new();
    let mut conf = vec![];
    let mut first = 0;
    let mut i = off + sps;
    while i < samples.len() {
        let (a, b) = (samples[i], samples[i - sps]);
        if a.norm() > strong && b.norm() > strong {
            if symbols.is_empty() {
                first = i;
            }
            let ang = (a * b.conj() * derot).arg();
            symbols.push(match ang {
                x if (0.0..PI / 2.0).contains(&x) => '0',
                x if x >= PI / 2.0 => '1',
                x if x < -PI / 2.0 => '2',
                _ => '3',
            });
            // the boundaries are every 90 degrees from 0; the ideal changes halfway between
            let to_boundary = (ang.rem_euclid(PI / 2.0)).min(PI / 2.0 - ang.rem_euclid(PI / 2.0));
            conf.push((to_boundary / (PI / 4.0) * 255.0).round().clamp(0.0, 255.0) as u8);
        } else if !symbols.is_empty() {
            symbols.push('.'); // a gap inside the burst
            conf.push(0);
        }
        i += sps;
    }
    let keep = symbols.trim_end_matches('.').len();
    symbols.truncate(keep);
    conf.truncate(keep);
    (symbols, conf, first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_are_planned_for_the_channels_wanted() {
        // two channels close together: the smallest rate, both clear of DC
        let (c, r) = plan_capture(&[102, 114]).unwrap();
        assert_eq!(r, 1.024e6);
        let heard: Vec<i64> = channels_in(c, r).iter().map(|&f| channel_number(f)).collect();
        assert!(heard.contains(&102) && heard.contains(&114), "{c} {heard:?}");
        // further apart needs a wider capture
        let (c, r) = plan_capture(&[102, 153]).unwrap();
        assert!(r > 1.024e6);
        let heard: Vec<i64> = channels_in(c, r).iter().map(|&f| channel_number(f)).collect();
        assert!(heard.contains(&102) && heard.contains(&153));
        // the whole band fits one dongle at 2.4 MS/s
        assert!(plan_capture(&[101, 176]).is_some());
        assert_eq!(plan_capture(&[]), None);
    }

    #[test]
    fn the_grid_and_the_band() {
        assert_eq!(channel_hz(channel_number(220.1375e6)), 220.1375e6);
        assert_eq!(channel_number(BAND_HI_HZ) - channel_number(BAND_LO_HZ) + 1, 76);
        let ch = channels_in(220.2875e6, 1.024e6);
        assert!(ch.contains(&220.1375e6) && ch.contains(&220.4375e6), "{ch:?}");
        assert!(!ch.iter().any(|f| (f - 220.2875e6).abs() < 15e3), "DC is avoided");
        assert_eq!(decimation(1.024e6), Some(8)); // 128 kS/s: 8 and 16 samples per symbol
        assert_eq!(decimation(2.4e6), Some(25)); // 96 kS/s
    }

    /// A pi/4-DQPSK burst: random phase changes, smooth pulses, an offset and noise.
    fn synth(symbols: &[u8], sr: f64, rate: f64, offset_hz: f64, noise: f32) -> Vec<Complex32> {
        let sps = (rate / sr) as usize;
        let mut phase = 0.0f32;
        let mut pts = vec![Complex32::new(1.0, 0.0)];
        for &s in symbols {
            phase += [PI / 4.0, 3.0 * PI / 4.0, -3.0 * PI / 4.0, -PI / 4.0][s as usize];
            pts.push(Complex32::from_polar(1.0, phase));
        }
        let mut seed = 7u32;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as f32 / u32::MAX as f32 - 0.5
        };
        let mut out = vec![Complex32::new(0.0, 0.0); 40];
        for w in pts.windows(2) {
            for k in 0..sps {
                // raised-cosine move from one point to the next
                let a = 0.5 - 0.5 * (PI * k as f32 / sps as f32).cos();
                out.push(w[0] * (1.0 - a) + w[1] * a);
            }
        }
        out.extend(vec![Complex32::new(0.0, 0.0); 40]);
        out.iter()
            .enumerate()
            .map(|(i, x)| x * Complex32::from_polar(1.0, (2.0 * std::f64::consts::PI * offset_hz * i as f64 / rate) as f32)
                + Complex32::new(rnd(), rnd()) * noise)
            .collect()
    }

    #[test]
    fn demodulates_a_clean_burst_at_either_rate() {
        let syms: Vec<u8> = (0..400u32).map(|i| ((i * 7 + i / 3) % 4) as u8).collect();
        for sr in [16_000.0, 8_000.0] {
            let d = demod(&synth(&syms, sr, 128_000.0, 300.0, 0.05), 128_000.0).unwrap();
            assert_eq!(d.symbol_rate, sr);
            assert!(d.quality > 0.8, "{d:?}");
            assert!((d.freq_offset_hz - 300.0).abs() < 60.0, "{}", d.freq_offset_hz);
            let want: String = syms.iter().map(|s| char::from(b'0' + s)).collect();
            // the first and last symbols sit on the burst's edges; the body must match
            assert!(d.symbols.contains(&want[5..390]), "{}\n{}", d.symbols, want);
        }
    }

    #[test]
    fn finds_a_burst_above_the_floor() {
        let rate = 128_000.0;
        let mut f = BurstFinder::new(220.1375e6, rate, 1000.0, 8.0);
        let mut seed = 1u32;
        let mut noise = |n: usize, amp: f32| -> Vec<Complex32> {
            (0..n)
                .map(|_| {
                    seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                    let a = (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
                    seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                    let b = (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
                    Complex32::new(a, b) * amp
                })
                .collect()
        };
        let mut s = noise(128_000, 0.01); // 1 s of noise to learn the floor
        let burst_at = s.len();
        s.extend(noise(6400, 0.01).iter().enumerate().map(|(i, c)| c + Complex32::from_polar(0.2, i as f32 * 0.3))); // 50 ms
        s.extend(noise(64_000, 0.01));
        let mut out = vec![];
        f.process(&s, &mut out);
        assert_eq!(out.len(), 1, "{:?}", out.iter().map(|b| b.duration_s).collect::<Vec<_>>());
        let b = &out[0];
        assert!((b.start - (1000.0 + burst_at as f64 / rate)).abs() < 0.006, "{}", b.start);
        assert!((b.duration_s - 0.05).abs() < 0.015, "{}", b.duration_s);
        assert!(b.peak_db > 20.0);
    }
}
