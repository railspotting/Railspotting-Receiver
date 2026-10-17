//! The simulator and the device-loop helpers (the dsp side of a device loop recovering after a lost dongle).

use super::*;
use crate::sdr::DeviceControl;
use crate::sim::{SimDevice, Traffic, World};
use railspotting_common::FakeClock;
use serde_json::json;

#[test]
fn sim_device_streams_until_cancelled_and_stream_ends_cleanly() {
    let tmp = TmpDir::new("sim-dev");
    let world = Arc::new(World::default());
    world.tone(160.95, 20.0, 1.0, None, 0.0);
    let (src, events) = source(tmp.path(), &vhf());
    let src = Arc::new(Mutex::new(src));
    let dev = Arc::new(SimDevice::new("30002222", world.clone(), FakeClock::new(0.0).clock(), false));
    let (rate, block) = {
        let mut s = src.lock().unwrap();
        let center = s.open_center();
        dev.configure(s.rate, center * 1e6, s.gain.clone());
        s.stream_started(dev.clone(), Some(dev.info.clone()));
        (s.rate, s.block)
    };
    assert!((dev.center_mhz() - 161.5452).abs() < 1e-9);
    let (s2, d2) = (src.clone(), dev.clone());
    let mut k = 0usize;
    let mut cb = move |iq: Vec<Complex32>| {
        k += 1;
        s2.lock().unwrap().process_block(&iq, k as f64 * block as f64 / rate);
        if k == 60 {
            d2.cancel_read_async(); // stream ends as if unplugged
        }
    };
    dev.read_samples_async(&mut cb, block);
    let mut s = src.lock().unwrap();
    assert!(s.connected && s.streaming && s.blocks == 60);
    assert_eq!(s.status()["connected"], true);
    assert_eq!(s.capability()["vendor"], "Nooelec");
    assert_eq!(states(&events), vec!["open"]);
    let reason = s.stream_end_reason();
    assert_eq!(reason, "stream ended");
    s.stream_ended(Some(reason));
    assert!(!s.connected && !s.streaming && s.dev.is_none());
    assert_eq!(states(&events), vec!["open", "closed"]); // the open event closed with the stream
    assert_eq!(s.last_error.as_deref(), Some("stream ended"));
    assert!(logged("VHF SDR connected: Nooelec NESDR SMArt v5, tuner R820T/R820T2, 161.5452 MHz, 2.88 MS/s, gain 20.7 dB"));
    assert!(logged("VHF SDR lost: stream ended"));
}

#[test]
fn sim_device_set_center_and_stall() {
    let world = Arc::new(World::default());
    let dev = Arc::new(SimDevice::new("x", world, FakeClock::new(0.0).clock(), false));
    dev.configure(2_400_000.0, 458.1385e6, json!(40.2));
    DeviceControl::set_center(&*dev, 457.0e6);
    assert_eq!(dev.center_mhz(), 457.0);
    dev.stall.store(true, std::sync::atomic::Ordering::SeqCst);
    let d2 = dev.clone();
    let stop = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(50));
        d2.cancel_read_async();
    });
    let mut n = 0;
    dev.read_samples_async(&mut |_| n += 1, 1024); // stalled: no blocks, but it still stops
    stop.join().unwrap();
    assert_eq!(n, 0);
}

#[test]
fn traffic_adds_reference_voice_and_eot() {
    let clock = FakeClock::new(1000.0);
    let world = Arc::new(World::default());
    let mut tr = Traffic::new(world.clone(), &plan("7"), clock.clock());
    tr.begin();
    let em = world.emitters();
    assert!(em.len() == 1 && em[0].kind == "noaa" && em[0].freq == 162.55 && em[0].end.is_none());
    clock.advance(16.0);
    tr.tick();
    let kinds: Vec<String> = world.emitters().iter().map(|e| e.kind.clone()).collect();
    assert_eq!(kinds, vec!["noaa", "telemetry"]);
    let eot = world.emitters()[1].clone();
    assert_eq!(eot.freq, 457.9375);
    // two packets back to back: 16 + 2 * (69 + 11 + 63 + 8) bits at 1200 bps
    assert!((eot.end.unwrap() - eot.start - (16.0 + 2.0 * 151.0) / 1200.0).abs() < 1e-9);
    clock.advance(5.0);
    tr.tick();
    assert!(world.emitters().iter().any(|e| e.kind == "voice"));
    assert!(logged("simulate: voice on "));
    assert!(world.scanner_rssi(Some(162.55), clock.get()) >= 385 - 8 + 80);
    assert_eq!(world.scanner_rssi(None, clock.get()), 0);
    clock.advance(1000.0);
    tr.tick();
    assert!(world.emitters().iter().all(|e| e.end.is_none() || e.end.unwrap() > clock.get() - 5.0));
}

#[test]
fn traffic_thread_starts_and_stops() {
    let world = Arc::new(World::default());
    let mut h = Traffic::new(world.clone(), &plan("7"), railspotting_common::system_clock()).spawn();
    for _ in 0..100 {
        if !world.emitters().is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    h.stop();
    assert_eq!(world.emitters().len(), 1);
}
