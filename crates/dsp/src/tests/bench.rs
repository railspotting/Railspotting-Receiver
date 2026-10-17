//! Throughput: seconds of processing per second of IQ. Run with
//! `cargo test -p railspotting-dsp bench -- --ignored --nocapture` (add --release for real numbers).

use super::*;
use crate::sdr::{SdrSource, Transcriber, TranscriptDone};
use num_complex::Complex32;
use railspotting_common::FakeClock;
use serde_json::json;
use std::time::Instant;

struct Instant0;
impl Transcriber for Instant0 {
    fn submit(&self, _audio: Vec<f32>, done: TranscriptDone, _tag: &str, _event: Option<Value>) {
        done(None);
    }
}

fn blocks_for(dev: &Value, seconds: f64, busy: &[f64]) -> (SdrSource, Vec<Vec<Complex32>>) {
    let tmp = std::env::temp_dir().join("rs-dsp-bench");
    let (_, emit) = events_sink();
    let tr: Arc<dyn Transcriber> = Arc::new(Instant0);
    let cfg = plan("bench");
    let mut src = SdrSource::new(dev, &json!({"record_bursts": false}), &tmp, emit, Some(tr), FakeClock::new(0.0).clock());
    src.set_plan(&channels(&cfg), Some(&cfg["reference"]));
    let w = World::new(5);
    w.tone(162.55, 30.0, 0.0, None, 1000.0);
    for f in busy {
        w.speechlike(*f, 25.0, 1.0, None, 2500.0, "voice"); // after the floors are learned
    }
    let n = (seconds * src.rate / src.block as f64).ceil() as usize;
    let dur = src.block as f64 / src.rate;
    let blocks = (0..n).map(|k| w.iq(src.center.unwrap(), src.rate, k as f64 * dur, src.block)).collect();
    (src, blocks)
}

fn time(label: &str, mut src: SdrSource, blocks: Vec<Vec<Complex32>>) -> f64 {
    let dur = src.block as f64 / src.rate;
    let t0 = Instant::now();
    for (k, b) in blocks.iter().enumerate() {
        src.process_block(b, (k + 1) as f64 * dur);
    }
    let per = t0.elapsed().as_secs_f64() / (blocks.len() as f64 * dur);
    eprintln!("{label}: {per:.4} s per second of IQ ({:.1}x real time), {} audio taps open", 1.0 / per,
              src.taps.len());
    per
}

#[test]
#[ignore]
fn bench_throughput() {
    // VHF at 2.88 MS/s: 8 detectors, the reference, and three voice calls open with audio taps
    let (src, blocks) = blocks_for(&vhf(), 4.0, &[160.95, 161.1, 161.31]);
    time("vhf 2.88 MS/s, 3 taps", src, blocks);
    let (src, blocks) = blocks_for(&vhf(), 4.0, &[]);
    time("vhf 2.88 MS/s, idle", src, blocks);
    // EOT at 2.4 MS/s: one detector and its burst recorder
    let (src, blocks) = blocks_for(&eot_dev(), 4.0, &[]);
    time("eot 2.4 MS/s", src, blocks);
    let raw = vec![127u8; 2 * 114_688];
    let mut out = Vec::new();
    let t0 = Instant::now();
    for _ in 0..25 {
        out.clear();
        crate::cu8_to_iq(&raw, &mut out);
    }
    eprintln!("cu8 conversion: {:.3} s per second of IQ",
              t0.elapsed().as_secs_f64() / (25.0 * 114_688.0 / 2_880_000.0));
}

#[test]
#[ignore]
fn bench_stages() {
    let rate = 2_880_000.0;
    let b = 114_688usize;
    let w = World::new(5);
    let x = w.iq(161.5452, rate, 0.0, b);
    let reps = 50;
    let secs = reps as f64 * b as f64 / rate;
    let t = |label: &str, f: &mut dyn FnMut()| {
        let t0 = Instant::now();
        for _ in 0..reps { f(); }
        eprintln!("{label}: {:.4} s/s", t0.elapsed().as_secs_f64() / secs);
    };
    let fft = rustfft::FftPlanner::<f32>::new().plan_fft_forward(4096);
    let mut buf = x.clone();
    let mut scratch = vec![Complex32::new(0.0, 0.0); fft.get_inplace_scratch_len()];
    t("fft", &mut || fft.process_with_scratch(&mut buf, &mut scratch));
    let mut sd = crate::bursts::ShiftDecimator::new(rate, 100_000.0);
    let mut out = vec![];
    t("shiftdecim", &mut || { out.clear(); sd.process_into(&x, &mut out); });
    let mut m = crate::bursts::Mixer::new(100_000.0, rate);
    t("mixer", &mut || m.process_into(&x, &mut out));
    let mut tap = crate::voice::AudioTap::new(rate, 100_000.0);
    t("audiotap", &mut || tap.feed(&x));
    t("lag1", &mut || { std::hint::black_box(crate::sdr::lag1_correlation(&x[..4096])); });
    let re: Vec<f32> = x.iter().step_by(4).map(|c| c.re).collect();
    t("flatness", &mut || { std::hint::black_box(crate::sdr::sample_flatness(&re)); });
    let mut v = vec![];
    t("world.iq", &mut || v = w.iq(161.5452, rate, 0.0, b));
}
