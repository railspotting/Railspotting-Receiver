//! A dongle stuck streaming random bytes is flagged, produces no
//! events, and is reopened.

use super::test_sdr::CountingDev;
use super::*;
use crate::sdr::*;
use crate::sim::{Rng, World};
use num_complex::Complex32;

/// What the stuck dongle sent: every byte value equally likely, scaled like `cu8_to_iq`.
fn random_bytes(rng: &mut Rng, n: usize) -> Vec<Complex32> {
    (0..n).map(|_| {
        let a = rng.integers(0, 256) as f32;
        let b = rng.integers(0, 256) as f32;
        Complex32::new((a - 127.5) / 127.5, (b - 127.5) / 127.5)
    }).collect()
}

fn looks_corrupt(z: &[Complex32]) -> bool {
    let re: Vec<f32> = z.iter().map(|c| c.re).collect();
    let im: Vec<f32> = z.iter().map(|c| c.im).collect();
    sample_flatness(&re).min(sample_flatness(&im)) >= CORRUPT_FLATNESS && lag1_correlation(z) <= CORRUPT_MAX_LAG1
}

fn clipped(z: Vec<Complex32>) -> Vec<Complex32> {
    z.into_iter().map(|c| Complex32::new(c.re.clamp(-1.0, 1.0), c.im.clamp(-1.0, 1.0))).collect()
}

#[test]
fn random_bytes_look_corrupt() {
    let mut rng = Rng::new(7);
    let z = random_bytes(&mut rng, 98_304);
    let re: Vec<f32> = z.iter().map(|c| c.re).collect();
    assert!(sample_flatness(&re) > 0.9 && lag1_correlation(&z) < 0.02);
    assert!(looks_corrupt(&z));
}

/// Noise, overload and strong carriers (the flattest real case found) are all real.
#[test]
fn real_signals_never_look_corrupt() {
    let mut rng = Rng::new(7);
    let (n, fs) = (98_304usize, 2.4e6);
    let noise = |rng: &mut Rng, s: f64| -> Vec<Complex32> {
        (0..n).map(|_| Complex32::new((rng.normal(0.0, s) / 2f64.sqrt()) as f32,
                                      (rng.normal(0.0, s) / 2f64.sqrt()) as f32)).collect()
    };
    assert!(!looks_corrupt(&noise(&mut rng, 0.05)));
    for s in [0.5, 1.0, 2.0, 4.0] { // overload of every degree
        assert!(!looks_corrupt(&clipped(noise(&mut rng, s))));
    }
    for i in 0..20 {
        let a = 0.2 + (4.0 - 0.2) * i as f64 / 19.0;
        for ns in [0.0, 0.1, 0.3, 0.6] {
            for f in [1e3, 300e3, 1.19e6] {
                let ph = rng.random();
                let nz = noise(&mut rng, ns);
                let z: Vec<Complex32> = (0..n).map(|k| {
                    let p = 2.0 * std::f64::consts::PI * (f * k as f64 / fs + ph);
                    Complex32::new((a * p.cos()) as f32, (a * p.sin()) as f32) + nz[k]
                }).collect();
                assert!(!looks_corrupt(&clipped(z)), "{a} {ns} {f}");
            }
        }
    }
}

fn feed(src: &mut SdrSource, rng: &mut Rng, blocks: usize, t0: f64) -> f64 {
    let dur = src.block as f64 / src.rate;
    for i in 0..blocks {
        let b = random_bytes(rng, src.block);
        src.process_block(&b, t0 + (i + 1) as f64 * dur);
    }
    t0 + blocks as f64 * dur
}

#[test]
fn corrupt_dongle_flagged_quietly_and_recovers() {
    let tmp = TmpDir::new("corrupt");
    let (mut src, events) = source(tmp.path(), &eot_dev());
    let dev = CountingDev::new();
    src.dev = Some(dev.clone());
    src.clock = railspotting_common::FakeClock::new(1_790_865_400.0).clock(); // a real wall time
    src.connected = true;
    src.streaming = true;
    let w = World::default();
    let mut rng = Rng::new(7);
    let t = run(&mut src, &w, 0.0, 3.0); // normal: floor learned, nothing flagged
    let floor = src.detector(457.9375).unwrap().floor;
    assert!(!src.corrupt && floor.is_some());

    let t = feed(&mut src, &mut rng, CORRUPT_BLOCKS as usize - 1, t);
    assert!(!src.corrupt); // not on a few blocks
    let t = feed(&mut src, &mut rng, 30, t);
    assert!(src.corrupt && src.status()["corrupt"] == true);
    assert_eq!(src.capability()["connected"], false);
    assert_eq!(dev.cancels(), 1); // one reopen attempt, not one per block
    assert!(all(&events).is_empty()); // no false events
    assert_eq!(src.detector(457.9375).unwrap().floor, floor);
    assert!(logged("dongle 30001111 is sending corrupt data"));

    run(&mut src, &w, t, t + 2.0); // unplugged and back: real samples again
    assert!(!src.corrupt && src.capability()["connected"] == true);
    assert!(logged("dongle 30001111 is sending normal samples again"));
}
