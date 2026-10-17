//! Voice audio tap: one NFM channel from the wideband stream to 16 kHz audio.

use num_complex::Complex32;

use crate::bursts::{bandpass, lowpass, FirDecimator, ShiftDecimator};

pub const AUDIO_RATE: f64 = 16_000.0;
pub const MAX_AUDIO_S: f64 = 120.0;

/// shift and decimate to 48 kS/s, 7 kHz low-pass, FM discriminator,
/// 300-3400 Hz band-pass, decimate by 3 to 16 kHz float32.
pub struct AudioTap {
    pub sd: ShiftDecimator,
    pub chan: FirDecimator<Complex32>,
    pub audio_filter: FirDecimator<f32>,
    pub rate: f64,
    pub prev: Option<Complex32>,
    pub max_samples: usize,
    audio: Vec<f32>,
    y48: Vec<Complex32>,
    y: Vec<Complex32>,
    disc: Vec<f32>,
}

impl AudioTap {
    pub fn new(rate: f64, offset_hz: f64) -> Self {
        Self::with_max(rate, offset_hz, MAX_AUDIO_S)
    }

    pub fn with_max(rate: f64, offset_hz: f64, max_s: f64) -> Self {
        let sd = ShiftDecimator::new(rate, offset_hz);
        let fs = sd.out_rate;
        let decim = ((fs / AUDIO_RATE).round_ties_even() as usize).max(1);
        let out_rate = fs / decim as f64;
        AudioTap {
            sd,
            chan: FirDecimator::new(&lowpass(63, 7_000.0, fs), 1),
            audio_filter: FirDecimator::new(&bandpass(255, 300.0, 3_400.0, fs), decim),
            rate: out_rate,
            prev: None,
            max_samples: (max_s * out_rate) as usize,
            audio: Vec::new(),
            y48: Vec::new(),
            y: Vec::new(),
            disc: Vec::new(),
        }
    }

    pub fn feed(&mut self, block: &[Complex32]) {
        if self.audio.len() >= self.max_samples {
            return;
        }
        self.y48.clear();
        self.sd.process_into(block, &mut self.y48);
        self.y.clear();
        self.chan.process_into(&self.y48, &mut self.y);
        if self.y.is_empty() {
            return;
        }
        let mut last = self.prev.unwrap_or(self.y[0]);
        self.disc.clear();
        for &v in &self.y {
            self.disc.push((v * last.conj()).arg());
            last = v;
        }
        self.prev = Some(last);
        self.audio_filter.process_into(&self.disc, &mut self.audio);
        self.audio.truncate(self.max_samples);
    }

    pub fn n(&self) -> usize {
        self.audio.len()
    }

    pub fn duration_s(&self) -> f64 {
        self.audio.len() as f64 / self.rate
    }

    pub fn audio(&self) -> Vec<f32> {
        self.audio.clone()
    }

    /// Take the audio so far, for a tap that streams (live listen) rather than records.
    pub fn drain(&mut self) -> Vec<f32> {
        std::mem::take(&mut self.audio)
    }
}

#[cfg(test)]
mod drain_tests {
    use super::*;

    #[test]
    fn drain_streams_the_audio_in_pieces() {
        let rate = 1_024_000.0;
        let mut tap = AudioTap::with_max(rate, 25_000.0, 1.0);
        // a carrier 25 kHz off centre, 0.1 s at a time
        let block: Vec<Complex32> = (0..102_400)
            .map(|i| Complex32::from_polar(1.0, (2.0 * std::f64::consts::PI * 25_000.0 * i as f64 / rate) as f32))
            .collect();
        tap.feed(&block);
        let first = tap.drain();
        assert!(!first.is_empty());
        assert_eq!(tap.n(), 0);
        // the cap counts what is held, so a streaming tap never stops at it
        for _ in 0..15 {
            tap.feed(&block);
            assert!(!tap.drain().is_empty());
        }
    }
}
