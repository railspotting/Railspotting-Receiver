//! The channelizer, detectors and SDR source (the parts the dsp crate owns).

use super::*;
use crate::sdr::*;
use crate::sim::{Emitter, World};
use serde_json::json;
use std::sync::atomic::AtomicUsize;

fn ch() -> Vec<Value> {
    channels(&plan("7"))
}

fn reference() -> Value {
    plan("7")["reference"].clone()
}

fn chs(v: &[Value]) -> Vec<i64> {
    v.iter().map(|c| c["ch"].as_i64().unwrap()).collect()
}

fn approx(a: f64, b: f64, tol: f64) -> bool {
    (a - b).abs() <= tol
}

// coverage and centering -------------------------------------------------------------------

#[test]
fn wanted_by_role() {
    let r = reference();
    let (c, rf) = wanted("vhf", &ch(), Some(&r));
    assert_eq!(chs(&c), vec![1, 2, 3, 4, 5, 6, 7, 8]);
    assert_eq!(rf, Some(r.clone()));
    assert_eq!(chs(&wanted("eot", &ch(), Some(&r)).0), vec![9]);
    assert_eq!(chs(&wanted("hot", &ch(), Some(&r)).0), vec![10]);
    let (c, rf) = wanted("uhf", &ch(), Some(&r));
    assert_eq!(chs(&c), vec![9, 10]);
    assert!(rf.is_none());
}

#[test]
fn frames_per_window_by_rate() {
    assert_eq!(frames_per_window(2_400_000.0), 12);
    assert_eq!(frames_per_window(2_880_000.0), 14);
}

#[test]
fn auto_center_upstate_vhf() {
    let r = reference();
    let mut freqs: Vec<f64> = wanted("vhf", &ch(), Some(&r)).0.iter().map(freq_of).collect();
    freqs.push(162.55);
    assert!(approx(auto_center(&freqs, half_span_mhz(2_880_000.0)).unwrap(), 161.5452, 1e-6));
}

#[test]
fn auto_center_lone_eot() {
    let c = auto_center(&[457.9375], half_span_mhz(2_400_000.0)).unwrap();
    assert!(approx(c, 458.1385, 1e-6));
    assert!(approx((c - 457.9375).abs(), 0.2, 0.002));
}

#[test]
fn every_channel_clear_of_center() {
    for (role, rate) in [("vhf", 2_880_000.0), ("eot", 2_400_000.0), ("hot", 2_400_000.0)] {
        let p = plan_window(role, rate, &ch(), Some(&reference()), None, None);
        assert!(!p.covered.is_empty() && p.uncovered.is_empty());
        let center = p.center.unwrap();
        for c in &p.covered {
            assert!((freq_of(c) - center).abs() >= 0.025 - 1e-9);
        }
        if let Some(r) = &p.reference {
            assert!((freq_of(r) - center).abs() >= 0.025);
        }
    }
}

#[test]
fn reference_dropped_then_uncovered_reported() {
    let mut wide = ch();
    wide.push(json!({"ch": 11, "freq": 159.81, "railroad": "X", "label": "far", "kind": "voice", "proximity": false}));
    let p = plan_window("vhf", 2_880_000.0, &wide, Some(&reference()), None, None);
    assert!(p.reference.is_none()); // channels fit, reference did not
    assert!(p.uncovered.is_empty());
    let mut wider = wide.clone();
    wider.push(json!({"ch": 12, "freq": 163.5, "railroad": "X", "label": "farther", "kind": "voice", "proximity": false}));
    let p = plan_window("vhf", 2_880_000.0, &wider, Some(&reference()), None, None);
    assert!(approx(p.center.unwrap(), (159.81 + 163.5) / 2.0, 1e-4));
    let mut un = chs(&p.uncovered);
    un.sort();
    assert_eq!(un, vec![2, 11, 12]); // 160.245 is 1.41 MHz out too
}

#[test]
fn fixed_center_overrides() {
    let p = plan_window("vhf", 2_880_000.0, &ch(), Some(&reference()), Some(161.0), None);
    assert_eq!(p.center, Some(161.0));
}

// detection ------------------------------------------------------------------------------------

#[test]
fn tone_opens_and_closes_with_sdr_rssi() {
    let tmp = TmpDir::new("tone");
    let (mut src, events) = source(tmp.path(), &vhf());
    let w = World::default();
    w.tone(160.95, 20.0, 2.0, Some(3.5), 0.0);
    run(&mut src, &w, 0.0, 5.0);
    assert_eq!(states(&events), vec!["open", "closed"]);
    let ev = all(&events);
    let (op, cl) = (&ev[0], &ev[1]);
    assert!(op["event_id"] == cl["event_id"] && cl["ch"] == 5 && cl["kind"] == "voice");
    assert!(approx(cl["start_ts"].as_f64().unwrap(), 2.0, 0.05));
    assert!(approx(cl["end_ts"].as_f64().unwrap(), 3.5, 0.05));
    let peak = cl["peak_rssi"].as_i64().unwrap();
    assert!((385 + 17..=385 + 23).contains(&peak), "{peak}"); // VHF nominal 385 plus SNR
    assert!(cl["samples"].as_i64().unwrap() > 50 && !cl.contains_key("truncated"));
}

#[test]
fn single_window_spike_does_not_open() {
    let mut det = ChannelDetector::new(160.95, json!(5), "voice", "x", 0.02);
    for i in 0..40 {
        det.feed(i as f64 * 0.02, -30.0 + (i % 3) as f64 * 0.1);
    }
    assert_eq!(det.feed(0.80, -10.0), None);
    for i in 41..80 {
        assert_eq!(det.feed(i as f64 * 0.02, -30.0), None);
    }
}

#[test]
fn short_burst_at_10db_opens() {
    let tmp = TmpDir::new("short");
    let (mut src, events) = source(tmp.path(), &eot_dev());
    let w = World::default();
    w.carrier(457.9375, 10.0, 2.0, Some(2.2), 0.0, 2500.0, "telemetry");
    run(&mut src, &w, 0.0, 3.0);
    let c = closed(&events);
    assert!(c.len() == 1 && c[0]["peak_rssi"].as_i64().unwrap() >= 230 + 8);
    assert!(c[0]["kind"] == "telemetry" && c[0]["ch"] == 9);
}

#[test]
fn noise_rise_on_eot_truncates_and_records_nothing() {
    let tmp = TmpDir::new("noise-rise");
    let (mut src, events) = source(tmp.path(), &eot_dev());
    let w = World::default();
    w.add(Emitter::new(457.9375, 9.5, 2.0, None, "noise"));
    run(&mut src, &w, 0.0, 9.0);
    let c = closed(&events);
    assert!(c.len() == 1 && c[0]["truncated"] == true);
    assert!(c[0]["end_ts"].as_f64().unwrap() - c[0]["start_ts"].as_f64().unwrap() < 3.5);
    let det = src.detector(457.9375).unwrap();
    assert!(det.floor.unwrap() > -80.0); // relearned at the new level
    assert!(!det.receiving());
    let bursts = tmp.path().join("bursts");
    assert!(!bursts.exists() || std::fs::read_dir(&bursts).unwrap()
        .all(|e| e.unwrap().path().extension().is_none_or(|x| x != "cf32")));
}

#[test]
fn rssi_scale_uhf_12db() {
    let mut det = ChannelDetector::new(457.9375, json!(9), "telemetry", "EOT", 0.02);
    for i in 0..25 {
        det.feed(i as f64 * 0.02, -50.0);
    }
    det.feed(0.5, -38.0);
    let action = det.feed(0.52, -38.0);
    assert_eq!(action, Some(DetAction::Open));
    let ev = det.event.clone().unwrap();
    assert_eq!(det.payload(&ev, "open")["peak_rssi"], 242);
}

// plan changes and streaming -------------------------------------------------------------------

#[test]
fn set_plan_keeps_floors_and_retunes() {
    let tmp = TmpDir::new("set-plan");
    let (mut src, _events) = source(tmp.path(), &vhf());
    let w = World::default();
    run(&mut src, &w, 0.0, 2.0);
    let floor = src.detector(160.905).unwrap().floor;
    assert!(floor.is_some());
    src.streaming = true;
    let mut cfg = plan("8");
    cfg["channels"][4]["freq"] = json!(160.935); // ch 5 moves; window does not
    src.set_plan(&channels(&cfg), Some(&cfg["reference"]));
    assert!(src.retune_to.is_none() && approx(src.center.unwrap(), 161.5452, 1e-9));
    assert_eq!(src.detector(160.905).unwrap().floor, floor);
    assert_eq!(src.detector(160.935).unwrap().floor, None);
    assert!(src.detector(160.95).is_none());
    let mut cfg = plan("9");
    cfg["channels"][0]["freq"] = json!(160.2); // outside the old window: center moves
    src.set_plan(&channels(&cfg), Some(&cfg["reference"]));
    assert!(src.retune_to.is_some() && !approx(src.center.unwrap(), 161.5452, 1e-6));
    assert!(src.detectors.iter().all(|d| d.floor.is_none()));
}

#[test]
fn plan_change_closes_open_event_of_removed_channel() {
    let tmp = TmpDir::new("plan-close");
    let (mut src, events) = source(tmp.path(), &vhf());
    let w = World::default();
    w.tone(160.95, 20.0, 1.0, None, 0.0);
    run(&mut src, &w, 0.0, 2.0);
    assert_eq!(all(&events).last().unwrap()["state"], "open");
    let cfg = plan("8");
    let chans: Vec<Value> = channels(&cfg).into_iter().filter(|c| c["ch"] != 5).collect();
    src.set_plan(&chans, Some(&cfg["reference"]));
    let last = all(&events).last().cloned().unwrap();
    assert!(last["state"] == "closed" && last["ch"] == 5);
}

pub struct CountingDev {
    pub cancels: AtomicUsize,
    pub centers: Mutex<Vec<f64>>,
}

impl CountingDev {
    pub fn new() -> Arc<Self> {
        Arc::new(CountingDev { cancels: AtomicUsize::new(0), centers: Mutex::new(vec![]) })
    }
    pub fn cancels(&self) -> usize {
        self.cancels.load(Ordering::SeqCst)
    }
}

impl DeviceControl for CountingDev {
    fn cancel_read_async(&self) {
        self.cancels.fetch_add(1, Ordering::SeqCst);
    }
    fn set_center(&self, hz: f64) {
        self.centers.lock().unwrap().push(hz);
    }
}

#[test]
fn stall_watchdog() {
    let tmp = TmpDir::new("stall");
    let (mut src, _) = source(tmp.path(), &vhf());
    let dev = CountingDev::new();
    src.dev = Some(dev.clone());
    src.streaming = true;
    src.last_block_rx = 100.0;
    assert!(!src.check_stall(105.0)); // blocks recent enough
    assert!(src.check_stall(110.5));
    assert!(dev.cancels() == 1 && src.stalls == 1);
    assert!(!src.check_stall(115.0)); // at most once per 10 s
    assert!(src.check_stall(121.0));
    src.last_block_rx = 125.0; // blocks flowing again
    assert!(!src.check_stall(130.0));
    src.streaming = false;
    assert!(!src.check_stall(200.0));
}

#[test]
fn retune_request_moves_dongle_and_skips_a_block() {
    let tmp = TmpDir::new("retune");
    let (mut src, _) = source(tmp.path(), &vhf());
    let dev = CountingDev::new();
    src.dev = Some(dev.clone());
    src.streaming = true;
    let mut cfg = plan("9");
    cfg["channels"][0]["freq"] = json!(160.2);
    src.set_plan(&channels(&cfg), Some(&cfg["reference"]));
    let target = src.retune_to.unwrap();
    let w = World::default();
    run(&mut src, &w, 0.0, 0.15); // 4 blocks
    assert_eq!(*dev.centers.lock().unwrap(), vec![target * 1e6]);
    assert!(src.retune_to.is_none());
    assert_eq!(src.blocks, 3); // the first block after the retune is dropped
}

#[test]
fn unplug_cancels_the_stream() {
    let tmp = TmpDir::new("unplug");
    let (mut src, _) = source(tmp.path(), &vhf());
    let dev = CountingDev::new();
    src.dev = Some(dev.clone());
    src.streaming = true;
    src.set_plugged_in(true, Some("1-1.2"));
    assert_eq!(dev.cancels(), 0); // first sighting is not news
    src.set_plugged_in(false, None);
    assert_eq!(dev.cancels(), 1);
    assert_eq!(src.status()["plugged_in"], false);
    assert_eq!(src.status()["usb_port"], "1-1.2");
}

// reference -----------------------------------------------------------------------------------------

#[test]
fn reference_snr_continuous() {
    let tmp = TmpDir::new("ref");
    let (mut src, _) = source(tmp.path(), &vhf());
    let w = World::default();
    w.tone(162.55, 35.0, 0.0, None, 0.0);
    run(&mut src, &w, 0.0, 1.0);
    let r = src.reference.clone().unwrap();
    assert!(r["name"] == reference()["name"] && r["freq"] == 162.55);
    let snr = r["snr_db"].as_f64().unwrap();
    assert!((30.0..=40.0).contains(&snr), "{snr}");
}

#[test]
fn capability_and_status_shape() {
    let tmp = TmpDir::new("cap");
    let (mut src, _) = source(tmp.path(), &eot_dev());
    src.connected = true;
    let c = src.capability();
    assert_eq!(c["channels"], json!([9]));
    assert_eq!(c["window_mhz"], json!([457.0345, 459.2425]));
    assert_eq!(c["connected"], true);
    assert!(src.covers_kind("telemetry", Some("EOT")) && !src.covers_kind("voice", None));
    let s = src.status();
    assert_eq!(s["center_mhz"], 458.1385);
    assert_eq!(s["channels"][0]["floor_db"], Value::Null);
    assert_eq!(src.measure(162.55), Err("UHF SDR is not connected".to_string()));
}

#[test]
fn repeated_frequency_keeps_first_position() {
    let tmp = TmpDir::new("dup-freq");
    let (mut src, _events) = source(tmp.path(), &vhf());
    let mut chans = channels(&plan("8"));
    let mut dup = chans[0].clone();
    dup["ch"] = json!(99);
    chans.push(dup);
    src.set_plan(&chans, None);
    let first = round_freq(&chans[0]);
    assert_eq!(src.detectors[0].freq, first);
    assert_eq!(src.detectors[0].ch, json!(99)); // the later entry wins, in the first place
    assert_eq!(src.detectors.iter().filter(|d| d.freq == first).count(), 1);
}

fn round_freq(c: &Value) -> f64 {
    crate::round_even(crate::sdr::freq_of(c), 4)
}
