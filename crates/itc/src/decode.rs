//! Samples in, decoded messages out: the channelizer, a burst finder per channel, the
//! demodulator and the link layer together, with no I/O. Feed it blocks from a dongle or a
//! recording; it hands back finished bursts and the messages in them (`crate::messages.md`).

use crate::dsp::{channel_number, channels_in, decimation, demod_burst_phases, Burst, BurstFinder, Channel};
use crate::link::{frames_soft, symbols_to_bits_conf, Frame};
use num_complex::Complex32;
use serde_json::{json, Value};

/// Every channel's channelizer and burst finder, for one capture.
pub struct Decoder {
    chans: Vec<(Channel, BurstFinder)>,
    cbuf: Vec<Complex32>,
    /// Samples near full scale since the last `take_clipped` (too much gain).
    clipped: usize,
}

impl Decoder {
    /// A capture at `center_hz` and `rate` whose first sample was taken at `start` (Unix seconds).
    /// `channels` are the channel frequencies to watch; empty means every one the capture hears.
    pub fn new(center_hz: f64, rate: f64, start: f64, channels: &[f64], open_db: f32) -> Result<Decoder, String> {
        let decim = decimation(rate).ok_or_else(|| format!("{} S/s does not divide into whole samples per symbol; use 1.024 MS/s, 2.048 MS/s or 2.4 MS/s", rate))?;
        let freqs = if channels.is_empty() { channels_in(center_hz, rate) } else { channels.to_vec() };
        let chans = freqs
            .iter()
            .map(|&f| {
                let ch = Channel::new(f, center_hz, rate, decim);
                let bf = BurstFinder::new(f, ch.out_rate, start, open_db);
                (ch, bf)
            })
            .collect();
        Ok(Decoder { chans, cbuf: vec![], clipped: 0 })
    }

    /// The channels watched, MHz.
    pub fn channels_mhz(&self) -> Vec<f64> {
        self.chans.iter().map(|(_, b)| b.hz() / 1e6).collect()
    }

    /// One block of samples: the bursts it finished, in time order.
    pub fn bursts(&mut self, block: &[Complex32]) -> Vec<Burst> {
        self.clipped += block.iter().filter(|c| c.re.abs() > 0.99 || c.im.abs() > 0.99).count();
        let mut out = vec![];
        for (ch, bf) in self.chans.iter_mut() {
            self.cbuf.clear();
            ch.process(block, &mut self.cbuf);
            bf.process(&self.cbuf, &mut out);
        }
        out.sort_by(|x, y| x.start.total_cmp(&y.start));
        out
    }

    /// Samples near full scale since the last call.
    pub fn take_clipped(&mut self) -> usize {
        std::mem::take(&mut self.clipped)
    }
}

/// A burst's place and level, as JSON.
pub fn burst_json(b: &Burst) -> Value {
    json!({
        "t": (b.start * 1000.0).round() / 1000.0,
        "ch_mhz": (b.channel_hz / 1e3).round() / 1e3,
        "ch": channel_number(b.channel_hz),
        "dur_ms": (b.duration_s * 1000.0).round(),
        "peak_db": (b.peak_db * 10.0).round() / 10.0,
        "floor_db": (b.floor_db * 10.0).round() / 10.0,
    })
}

/// Demodulate a burst and decode its frames, as JSON (`messages.md`). Every sampling phase and
/// every transmission in the burst is tried; each good frame is kept once. With `all`, frames
/// that failed the CRC are kept too.
pub fn decode_burst(b: &Burst, all: bool) -> Vec<Value> {
    // (frame, its time, its symbol rate)
    let mut found: Vec<(Frame, f64, f64)> = vec![];
    for d in demod_burst_phases(b) {
        let (bits, conf) = symbols_to_bits_conf(&d.symbols, &d.conf);
        for f in frames_soft(&bits, Some(&conf)) {
            // the frame's first bit is symbol bit/2, taken at sample first + that * sps
            let t = b.start + (d.first as f64 + (f.bit / 2) as f64 * d.sps as f64) / b.rate;
            let same = |(g, u, _): &(Frame, f64, f64)| (u - t).abs() < 0.002 && (g.data == f.data || !g.crc_ok || !f.crc_ok);
            match found.iter().position(same) {
                Some(k) if !found[k].0.crc_ok && f.crc_ok => found[k] = (f, t, d.symbol_rate),
                Some(_) => {}
                None => found.push((f, t, d.symbol_rate)),
            }
        }
    }
    found.sort_by(|a, b| a.1.total_cmp(&b.1));
    found
        .into_iter()
        .filter(|(f, _, _)| all || f.crc_ok)
        .map(|(f, t, rate)| {
            let mut j = crate::msg::describe(f.header.kind, f.payload());
            j["t"] = json!((t * 1000.0).round() / 1000.0);
            j["ch"] = json!(channel_number(b.channel_hz));
            j["ch_mhz"] = json!((b.channel_hz / 1e3).round() / 1e3);
            j["rate"] = json!(if rate > 12_000.0 { "FR" } else { "HR" });
            j["kind"] = json!(f.header.kind);
            j["len"] = json!(f.header.length);
            j["crc_ok"] = json!(f.crc_ok);
            j["corrected"] = json!(f.corrected);
            j["level_db"] = json!((b.peak_db * 10.0).round() / 10.0);
            j["floor_db"] = json!((b.floor_db * 10.0).round() / 10.0);
            if !f.trailer().is_empty() {
                j["trailer"] = json!(f.trailer().iter().map(|x| format!("{x:02x}")).collect::<String>());
            }
            j
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::channel_hz;
    use std::f32::consts::PI;

    /// What a dongle would give: noise, then one ITC frame on a channel away from the centre,
    /// then noise.
    fn capture(center: f64, rate: f64, channel: f64, bits: &[u8]) -> Vec<Complex32> {
        let sps = (rate / 16_000.0) as usize;
        let shift = 2.0 * PI * (channel - center) as f32 / rate as f32;
        let mut seed = 12345u32;
        let mut noise = move || {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            ((seed >> 16) as f32 / 32768.0 - 1.0) * 0.02
        };
        let mut clean = vec![Complex32::new(0.0, 0.0); (1.5 * rate) as usize];
        let mut phase = 0.0f32;
        for d in bits.chunks(2) {
            phase += match (d[0], d[1]) {
                (0, 0) => PI / 4.0,
                (0, 1) => 3.0 * PI / 4.0,
                (1, 1) => -3.0 * PI / 4.0,
                _ => -PI / 4.0,
            };
            clean.extend(std::iter::repeat_n(Complex32::from_polar(0.4, phase), sps));
        }
        clean.extend(vec![Complex32::new(0.0, 0.0); (0.5 * rate) as usize]);
        clean.iter().enumerate().map(|(n, v)| v * Complex32::from_polar(1.0, shift * n as f32) + Complex32::new(noise(), noise())).collect()
    }

    #[test]
    fn a_captured_frame_comes_out_as_a_message() {
        // a made-up wayside status: group 123456, unit 0ff0, indicators b600
        let msg = [0x12, 0x34, 0x56, 0x0f, 0xf0, 0x82, 0x07, 0x50, 0xb6, 0x00, 1, 2, 3, 4];
        let mut bits = vec![1u8; 14];
        bits.extend(crate::link::encode_frame(0x34, &msg));
        let ch114 = channel_hz(114);
        let samples = capture(220.3e6, 1.024e6, ch114, &bits);
        let mut dec = Decoder::new(220.3e6, 1.024e6, 1000.0, &[ch114, channel_hz(102)], 8.0).unwrap();
        let mut got = vec![];
        // in 50 ms blocks, as a dongle delivers them
        for block in samples.chunks(51_200) {
            for b in dec.bursts(block) {
                got.extend(decode_burst(&b, false));
            }
        }
        assert_eq!(got.len(), 1, "{got:?}");
        let m = &got[0];
        assert_eq!(m["type"], "WIU_REPLY");
        assert_eq!(m["ch"], 114);
        assert_eq!(m["rate"], "FR");
        assert_eq!(m["wiu"], "1234560ff0");
        assert_eq!(m["indicators"], "b600");
        // the frame starts 1.5 s in, after the preamble
        assert!((m["t"].as_f64().unwrap() - 1001.5).abs() < 0.01, "{}", m["t"]);
        assert_eq!(dec.take_clipped(), 0);
    }

    #[test]
    fn a_rate_with_no_whole_samples_per_symbol_is_refused() {
        assert!(Decoder::new(220.3e6, 1.0e6, 0.0, &[], 8.0).is_err());
    }
}
