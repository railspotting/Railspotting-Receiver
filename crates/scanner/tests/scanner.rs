//! SR30C driver, monitor, focus, roles and checks against the fake SR30C.
use railspotting_common::{builtin_config, FakeClock};
use railspotting_scanner::checks;
use railspotting_scanner::fake::{CarrierWorld, FakeSR30C};
use railspotting_scanner::focus::{desired_role, AutoEotWatch, FocusManager, RoleManager, FOCUS_BANKS, FOCUS_CH};
use railspotting_scanner::monitor::{Hooks, ScannerMonitor};
use railspotting_scanner::scanner::ScannerError;
use railspotting_scanner::{fake_sleep, CommandHost, CommandRunner, Timing};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rs-scanner-it-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn plan(version: &str) -> Value {
    let mut cfg = builtin_config();
    cfg["config_version"] = json!(version);
    cfg
}

struct Rig {
    clock: FakeClock,
    world: Arc<CarrierWorld>,
    fake: FakeSR30C,
    mon: Arc<ScannerMonitor>,
    events: Arc<Mutex<Vec<Value>>>,
    cfg: Value,
    focus: Arc<FocusManager>,
    state: PathBuf,
}

impl Rig {
    fn t(&self) -> Timing {
        Timing::fake(&self.clock)
    }
    fn now(&self) -> f64 {
        self.clock.get()
    }
    fn events(&self) -> Vec<Value> {
        self.events.lock().unwrap().clone()
    }
    fn load_plan(&self) {
        checks::plan_sync(&self.mon, &self.cfg, &self.state).unwrap();
    }
    fn active(&self) -> Option<Value> {
        self.focus.active()
    }
}

fn hooks_for(clock: &FakeClock, fake: &FakeSR30C, events: Arc<Mutex<Vec<Value>>>, cfg: Value) -> Hooks {
    Hooks {
        emit: Arc::new(move |ev| events.lock().unwrap().push(ev)),
        get_config: Arc::new(move || cfg.clone()),
        opener: fake.opener(),
        finder: Arc::new(|| Some("/dev/ttyUSB0".to_string())),
        clock: clock.clock(),
        sleep: fake_sleep(clock),
        on_connect: None,
    }
}

fn rig(name: &str) -> Rig {
    let state = tmp(name);
    let clock = FakeClock::new(1000.0);
    let world = Arc::new(CarrierWorld::default());
    let fake = FakeSR30C::new(world.clone(), clock.clock());
    let events = Arc::new(Mutex::new(Vec::new()));
    let cfg = plan("7");
    let mon = Arc::new(ScannerMonitor::new("auto", 57600, hooks_for(&clock, &fake, events.clone(), cfg.clone())));
    assert!(mon.try_connect());
    let focus = Arc::new(FocusManager::new(mon.clone(), &state, clock.clock()));
    Rig { clock, world, fake, mon, events, cfg, focus, state }
}

fn read_json(p: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

// driver ------------------------------------------------------------------------------------------

#[test]
fn never_sends_clr_or_number_keys() {
    let r = rig("clr");
    assert!(r.mon.with_scanner(|s| s.cmd("CLR")).unwrap().is_err());
    assert!(r.mon.with_scanner(|s| s.cmd("clr,1")).unwrap().is_err());
    assert!(r.mon.with_scanner(|s| s.key("1")).unwrap().is_err());
    assert!(!r.fake.lock().log.iter().any(|l| l.to_uppercase().starts_with("CLR")));
}

#[test]
fn disconnect_reports_and_reconnects() {
    let r = rig("disc");
    r.fake.set_connected(false);
    let e = r.mon.poll_once().unwrap_err();
    assert_eq!(e, ScannerError::Io("serial error: device disconnected".into()));
    r.mon.disconnect("gone");
    assert_eq!(r.mon.heartbeat_info()["connected"], false);
    assert_eq!(r.mon.heartbeat_info()["model"], Value::Null);
    assert!(!r.mon.try_connect());
    assert_eq!(r.mon.last_error().as_deref(), Some("could not open port /dev/ttyUSB0"));
    r.fake.set_connected(true);
    assert!(r.mon.try_connect() && r.mon.connected());
    let hb = r.mon.heartbeat_info();
    assert_eq!(hb, json!({"connected": true, "model": "SR30C", "firmware": "Version 1.01.04", "cable_serial": null,
                          "port": null, "state": "scanning", "busy_reason": null, "freq": null,
                          "rssi": null, "volume": 9, "squelch": 2}));
    assert_eq!(r.mon.local_info()["port"], "/dev/ttyUSB0");
}

// events from squelch ----------------------------------------------------------------------------

#[test]
fn scanner_voice_event_from_squelch() {
    let r = rig("voice");
    r.load_plan();
    r.world.carrier(160.95, 30.0, r.now() + 1.0, Some(r.now() + 3.0));
    for _ in 0..150 {
        r.mon.poll_once().unwrap();
        r.clock.advance(0.03);
    }
    let ev = r.events();
    assert_eq!(ev.iter().map(|e| e["state"].as_str().unwrap()).collect::<Vec<_>>(), vec!["open", "closed"]);
    assert_eq!(ev[1]["ch"], 5);
    assert_eq!(ev[1]["kind"], "voice");
    assert!(ev[1]["peak_rssi"].as_i64().unwrap() > 440);
    let dur = ev[1]["end_ts"].as_f64().unwrap() - ev[1]["start_ts"].as_f64().unwrap();
    assert!((dur - 2.0).abs() <= 0.1, "{dur}");
    assert_eq!(ev[0]["event_id"], ev[1]["event_id"]);
    assert_eq!(ev[1]["_source"], "scanner");
    assert_eq!(ev[1]["_label"], "Charlotte Dist. Road");
    let keys: Vec<&String> = ev[1].as_object().unwrap().keys().collect();
    assert_eq!(keys, vec!["event_id", "state", "start_ts", "end_ts", "freq", "ch", "kind", "peak_rssi", "avg_rssi",
                          "samples", "_source", "_label", "_proximity"]);
}

// plan sync, EPG park, SCG --------------------------------------------------------------------------

#[test]
fn plan_sync_writes_only_differences() {
    let r = rig("sync");
    r.fake.program_channel(1, 160.905, true, false); // already right
    r.fake.program_channel(9, 457.9375, true, false); // telemetry must have delay off
    r.fake.program_channel(3, 161.19, true, true);
    r.fake.lock().writes = 0;
    let rep = checks::plan_sync(&r.mon, &r.cfg, &r.state).unwrap().unwrap();
    let fixed: Vec<&str> = rep["fixed"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert!(!fixed.contains(&"CH 1 set to 160.9050 MHz"));
    assert!(fixed.contains(&"CH 9 set to 457.9375 MHz") && fixed.contains(&"CH 3 set to 161.1900 MHz"));
    {
        let f = r.fake.lock();
        assert_eq!(f.writes as usize, fixed.len());
        assert_eq!(fixed.len(), 9);
        assert_eq!(f.channels[&9].delay, "0");
        assert_eq!(f.channels[&5].delay, "1");
        assert_eq!(f.channels[&3].lockout, "0");
        assert_eq!(f.mode, "scanning"); // EPG parked it; KEY,H,P resumed
        assert_eq!(f.log[f.log.len() - 2..], ["EPG".to_string(), "KEY,H,P".to_string()]);
    }
    assert!(checks::plan_sync(&r.mon, &r.cfg, &r.state).unwrap().is_none()); // same version: nothing
    let mut cfg2 = plan("8");
    let chans: Vec<Value> = cfg2["channels"].as_array().unwrap().iter().filter(|c| c["ch"] != 4).cloned().collect();
    cfg2["channels"] = json!(chans);
    cfg2["channels"][0]["freq"] = json!(160.920);
    r.fake.lock().writes = 0;
    let rep = checks::plan_sync(&r.mon, &cfg2, &r.state).unwrap().unwrap();
    assert_eq!(rep["fixed"], json!(["CH 1 set to 160.9200 MHz", "CH 4 cleared"]));
    assert_eq!(r.fake.lock().writes, 2);
    assert_eq!(r.fake.lock().channels[&4].freq, 0);
    assert_eq!(read_json(&r.state.join("scanner_plan.json"))["managed"], json!([1, 2, 3, 5, 6, 7, 8, 9, 10]));
}

#[test]
fn scan_setup_fixes_skipped_bank_1() {
    let r = rig("setup");
    r.load_plan();
    r.fake.lock().banks = "1111110110".into();
    let rep = checks::scan_setup_check(&r.mon, &r.cfg).unwrap();
    assert_eq!(rep["banks"], "0111110110");
    assert_eq!(r.fake.lock().banks, "0111110110");
    assert_eq!(rep["fixed"], json!(["banks 1111110110 -> 0111110110 (bank 1 = first digit, 0 = scanned)"]));
    assert_eq!(rep["problems"], json!([]));
    assert_eq!(r.fake.lock().mode, "scanning");
}

#[test]
fn scan_setup_reports_wrong_memory() {
    let r = rig("setup2");
    r.load_plan();
    r.fake.program_channel(2, 160.5, true, false);
    r.fake.program_channel(3, 161.19, true, true);
    let rep = checks::scan_setup_check(&r.mon, &r.cfg).unwrap();
    assert_eq!(rep["problems"], json!(["CH 2 holds 160.5, plan says 160.2450", "CH 3 (161.1900) is locked out"]));
}

// checks ----------------------------------------------------------------------------------------

#[test]
fn radio_check_restores_channel_1() {
    let r = rig("rc");
    r.load_plan();
    r.world.carrier(162.55, 36.0, 0.0, None);
    let data = checks::radio_check_scanner(&r.mon, &r.state, &r.cfg["reference"], &r.t()).unwrap();
    assert_eq!(data["ok"], true);
    assert!(data["rssi"].as_i64().unwrap() > 450);
    assert_eq!(data["squelch_open"], 1.0);
    let f = r.fake.lock();
    assert_eq!(f.channels[&1].freq, 1609050);
    assert_eq!(f.channels[&1].delay, "1");
    assert_eq!(f.mode, "scanning");
    assert!(!r.state.join("scanner_restore.json").exists());
}

#[test]
fn radio_check_fails_when_weak() {
    let r = rig("weak");
    r.load_plan();
    let data = checks::radio_check_scanner(&r.mon, &r.state, &r.cfg["reference"], &r.t()).unwrap();
    assert_eq!(data["ok"], false);
    let err = data["error"].as_str().unwrap();
    assert!(err.starts_with("NOAA WXJ21 Paris Mountain weak: RSSI ") && err.ends_with(", squelch open 0%"), "{err}");
}

#[test]
fn radio_check_without_scanner_is_a_failed_check() {
    let r = rig("rcnone");
    r.mon.disconnect("gone");
    let data = checks::radio_check_scanner(&r.mon, &r.state, &r.cfg["reference"], &r.t()).unwrap();
    assert_eq!(data, json!({"freq": 162.55, "rssi": null, "squelch_open": null, "ok": false, "error": "no scanner connected"}));
}

#[test]
fn radio_check_sdr() {
    let ok = checks::radio_check_sdr(&json!({"freq": 162.55, "snr_db": 31.4}), "NOAA");
    assert_eq!(ok, json!({"freq": 162.55, "rssi": 416, "squelch_open": 1.0, "ok": true, "error": null}));
    let bad = checks::radio_check_sdr(&json!({"freq": 162.55, "snr_db": 4.5}), "NOAA");
    assert_eq!(bad["error"], "NOAA only +4 dB on the SDR"); // half to even
    assert_eq!(bad["rssi"], 390);
}

#[test]
fn channel_1_restored_after_crash() {
    let r = rig("crash");
    r.load_plan();
    // a check wrote the reference into channel 1 and the process died
    std::fs::write(r.state.join("scanner_restore.json"),
                   json!({"channels": {"1": {"ch": 1, "freq": 160.905, "delay": "1", "lockout": false}}, "scg": null}).to_string()).unwrap();
    r.fake.program_channel(1, 162.55, false, false);
    assert!(checks::recover_after_crash(&r.mon, &r.state).unwrap());
    let f = r.fake.lock();
    assert_eq!(f.channels[&1].freq, 1609050);
    assert_eq!(f.channels[&1].delay, "1");
    assert!(!r.state.join("scanner_restore.json").exists());
    assert_eq!(f.mode, "scanning");
}

#[test]
fn detection_test_pass_and_fail() {
    let r = rig("detect");
    r.load_plan();
    let banks = r.fake.lock().banks.clone();
    let res = checks::detection_test(&r.mon, &r.state, &r.cfg, 10.0, &r.t()).unwrap();
    assert_eq!(res["ok"], false);
    assert_eq!(res["error"], "scan never stopped on 162.5500 MHz in 10 s");
    assert_eq!(res["channel"], 50);
    r.world.carrier(162.55, 36.0, 0.0, None);
    let res = checks::detection_test(&r.mon, &r.state, &r.cfg, 10.0, &r.t()).unwrap();
    assert_eq!(res["ok"], true);
    assert!(res["detect_s"].is_number() && res["peak_rssi"].as_i64().unwrap() > 450);
    let f = r.fake.lock();
    assert_eq!(f.channels[&50].freq, 0); // put back
    assert_eq!(f.banks, banks);
}

#[test]
fn reception_test_verdict() {
    let r = rig("reception");
    r.load_plan();
    r.world.carrier(162.55, 36.0, 0.0, None);
    let rep = checks::reception_test(&r.mon, &r.state, &r.cfg, 1.0, &r.t()).unwrap();
    assert_eq!(rep["verdict"], "good");
    assert!(rep["reference"]["margin"].as_i64().unwrap() >= 50);
    let mut keys: Vec<&String> = rep["channels"].as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(keys, vec!["160.2450", "160.9050", "160.9500", "161.1000", "161.1900", "161.2500", "161.3100", "161.3700"]);
    assert!(rep["detail"].as_str().unwrap().starts_with("NOAA WXJ21 Paris Mountain "));
}

#[test]
fn measure_many_keeps_dict_order() {
    let r = rig("many");
    r.load_plan();
    let m = checks::measure_many(&r.mon, &r.state, &[162.55, 160.905, 162.55], 0.2, &r.t(), "survey").unwrap();
    assert_eq!(m.iter().map(|(f, _)| *f).collect::<Vec<_>>(), vec![162.55, 160.905]);
    assert!(m.iter().all(|(_, s)| s.samples == 2));
    assert_eq!(r.fake.lock().channels[&1].freq, 1609050); // put back
}

#[test]
fn channel_memory() {
    let r = rig("mem");
    r.load_plan();
    let mem = checks::channel_memory(&r.mon).unwrap()["channels"].as_array().unwrap().clone();
    assert_eq!(mem.len(), 50);
    assert_eq!(mem[0], json!({"ch": 1, "freq": 160.905, "lockout": false}));
    assert_eq!(mem[20]["lockout"], true);
}

// focus ------------------------------------------------------------------------------------------

#[test]
fn focus_start_end() {
    let r = rig("focus");
    r.load_plan();
    r.focus.start(452.9375, None, "role", false, None).unwrap();
    {
        let f = r.fake.lock();
        assert_eq!(f.channels[&FOCUS_CH].freq, 4529375);
        assert_eq!(f.channels[&FOCUS_CH].delay, "0");
        assert_eq!(f.banks, FOCUS_BANKS);
        assert_eq!(f.mode, "scanning");
    }
    let saved = read_json(&r.state.join("focus.json"));
    assert_eq!(saved["original_scg"], "0111110110");
    assert_eq!(r.mon.state(), "parked");
    assert_eq!(r.mon.heartbeat_info()["state"], "parked");
    r.focus.end("").unwrap();
    assert_eq!(r.fake.lock().banks, "0111110110");
    assert_eq!(r.fake.lock().channels[&FOCUS_CH].freq, 0);
    assert!(!r.state.join("focus.json").exists());
}

#[test]
fn focus_resume_and_undo_after_restart() {
    let r = rig("resume");
    r.load_plan();
    r.focus.start(457.9375, Some(30.0), "auto_eot", false, None).unwrap();
    let again = FocusManager::new(r.mon.clone(), &r.state, r.clock.clock());
    again.recover().unwrap();
    assert_eq!(again.active().unwrap()["freq"], 457.9375);
    assert_eq!(r.fake.lock().banks, FOCUS_BANKS);
    r.clock.advance(31.0 * 60.0);
    let third = FocusManager::new(r.mon.clone(), &r.state, r.clock.clock());
    third.recover().unwrap();
    assert!(third.active().is_none());
    assert_eq!(r.fake.lock().banks, "0111110110");
}

#[test]
fn focus_tick_ends_when_time_is_up() {
    let r = rig("tick");
    r.load_plan();
    r.focus.start(457.9375, Some(1.0), "auto_eot", false, None).unwrap();
    r.clock.advance(59.0);
    r.focus.tick().unwrap();
    assert!(r.active().is_some());
    r.clock.advance(1.0);
    r.focus.tick().unwrap();
    assert!(r.active().is_none());
}

#[test]
fn rssi_trigger_needs_two_polls() {
    let r = rig("trigger");
    r.load_plan();
    r.focus.start(457.9375, None, "role", true, None).unwrap();
    let floor = r.mon.trigger_floor().unwrap();
    assert!((200.0..=260.0).contains(&floor), "{floor}");
    // one strong poll: nothing; squelch never opens (weak EOT)
    r.world.carrier(457.9375, 8.0, r.now(), Some(r.now() + 0.031));
    r.mon.poll_once().unwrap();
    r.clock.advance(0.05);
    r.mon.poll_once().unwrap();
    assert!(r.events().is_empty());
    r.world.carrier(457.9375, 8.0, r.now(), Some(r.now() + 0.2));
    for _ in 0..4 {
        r.mon.poll_once().unwrap();
        r.clock.advance(0.03);
    }
    let ev = r.events();
    assert!(!ev.is_empty() && ev[0]["state"] == "open");
    assert_eq!(ev[0]["kind"], "telemetry");
    assert_eq!(ev[0]["ch"], 9);
}

// roles ------------------------------------------------------------------------------------------

#[test]
fn role_table() {
    // what no SDR covers: EOT, then HOT, then the voice channels
    for (eot, hot, role) in [(false, false, "eot"), (false, true, "eot"), (true, false, "hot"), (true, true, "scan")] {
        assert_eq!(desired_role("auto", eot, hot).0, role);
        assert_eq!(desired_role("eot", eot, hot).0, "eot"); // receiver.json still decides
    }
    assert_eq!(desired_role("", false, false), ("eot".to_string(), "no SDR is on EOT; EOT is the priority".to_string()));
    // seen on 2026-10-15: one SDR on EOT and the scanner on EOT too; now the scanner takes HOT
    assert_eq!(desired_role("auto", true, false).0, "hot");
}

#[test]
fn role_hold_15s_and_parking() {
    let r = rig("roles");
    r.load_plan();
    let cfg = &r.cfg;
    let mut roles = RoleManager::new(Some(r.focus.clone()), r.clock.clock());
    assert_eq!(roles.tick("hot", "voice and EOT are on SDRs", cfg, false).unwrap(), None);
    r.clock.advance(10.0);
    assert_eq!(roles.tick("hot", "x", cfg, false).unwrap(), None);
    r.clock.advance(6.0);
    assert_eq!(roles.tick("hot", "x", cfg, false).unwrap().as_deref(), Some("hot"));
    assert_eq!(r.active().unwrap()["freq"], 452.9375);
    // a flaky link flips the desired role briefly: no change
    assert_eq!(roles.tick("eot", "x", cfg, false).unwrap(), None);
    r.clock.advance(5.0);
    assert_eq!(roles.tick("hot", "x", cfg, false).unwrap(), None);
    r.clock.advance(20.0);
    assert_eq!(roles.tick("hot", "x", cfg, false).unwrap(), None);
    assert_eq!(r.active().unwrap()["freq"], 452.9375);
    // UHF dongle unplugged for good: moves to EOT after 15 s
    roles.tick("eot", "voice is on an SDR; EOT is the priority", cfg, false).unwrap();
    r.clock.advance(16.0);
    assert_eq!(roles.tick("eot", "x", cfg, false).unwrap().as_deref(), Some("eot"));
    assert_eq!(r.active().unwrap()["freq"], 457.9375);
    r.clock.advance(1.0);
    roles.tick("scan", "no SDR connected", cfg, false).unwrap();
    r.clock.advance(16.0);
    assert_eq!(roles.tick("scan", "x", cfg, false).unwrap().as_deref(), Some("scan"));
    assert!(r.active().is_none());
}

#[test]
fn role_never_overrides_manual_focus() {
    let r = rig("manual");
    r.load_plan();
    r.focus.start(160.95, None, "manual", false, None).unwrap();
    let mut roles = RoleManager::new(Some(r.focus.clone()), r.clock.clock());
    roles.tick("hot", "x", &r.cfg, false).unwrap();
    r.clock.advance(20.0);
    assert_eq!(roles.tick("hot", "x", &r.cfg, false).unwrap(), None);
    assert_eq!(r.active().unwrap()["reason"], "manual");
    r.focus.end("").unwrap();
    assert_eq!(roles.tick("hot", "x", &r.cfg, false).unwrap().as_deref(), Some("hot"));
}

#[test]
fn role_without_scanner_is_remembered() {
    let r = rig("noscan");
    r.mon.disconnect("gone");
    let mut roles = RoleManager::new(Some(r.focus.clone()), r.clock.clock());
    roles.tick("eot", "x", &r.cfg, false).unwrap();
    r.clock.advance(16.0);
    assert_eq!(roles.tick("eot", "x", &r.cfg, false).unwrap(), None);
    assert_eq!(roles.role.as_deref(), Some("eot"));
}

// auto EOT watch ---------------------------------------------------------------------------------------

fn ev(ch: i64, freq: f64, state: &str, rssi: i64) -> Value {
    json!({"event_id": "e", "state": state, "freq": freq, "ch": ch, "peak_rssi": rssi, "kind": "voice"})
}

#[test]
fn auto_eot_triggers() {
    let r = rig("autotrig");
    r.load_plan();
    let w = AutoEotWatch::new(Some(&json!({"minutes": 30, "road_min_rssi": 420})), Some(r.focus.clone()), r.clock.clock());
    let cfg = &r.cfg;
    assert_eq!(w.trigger_for(&ev(10, 452.9375, "open", 300), cfg).as_deref(), Some("HOT"));
    assert_eq!(w.trigger_for(&ev(5, 160.95, "closed", 431), cfg).as_deref(), Some("road channel Charlotte Dist. Road"));
    assert_eq!(w.trigger_for(&ev(5, 160.95, "closed", 419), cfg), None);
    assert_eq!(w.trigger_for(&ev(6, 161.31, "closed", 600), cfg), None); // dispatcher
    assert_eq!(w.trigger_for(&ev(3, 161.19, "closed", 600), cfg), None); // yard
    assert_eq!(w.trigger_for(&ev(5, 160.95, "open", 600), cfg), None); // road triggers on close
}

#[test]
fn auto_eot_only_in_scan_role_without_sdr_eot() {
    let r = rig("autoeot");
    r.load_plan();
    let mut w = AutoEotWatch::new(Some(&json!({})), Some(r.focus.clone()), r.clock.clock());
    let road = ev(5, 160.95, "closed", 450);
    assert!(!w.consider(&road, &r.cfg, Some("hot"), false));
    assert!(!w.consider(&road, &r.cfg, Some("scan"), true));
    assert!(w.consider(&road, &r.cfg, Some("scan"), false));
    for _ in 0..100 {
        if r.active().is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let a = r.active().unwrap();
    assert_eq!(a["freq"], 457.9375);
    assert_eq!(a["reason"], "auto_eot");
    let span = a["end_ts"].as_f64().unwrap() - a["start_ts"].as_f64().unwrap();
    assert!((span - 1800.0).abs() <= 1.0, "{span}");
    let runs = w.runs();
    assert_eq!(runs.len(), 1);
    assert!(runs[0]["trigger"].as_str().unwrap().starts_with("road"));
    // EOT bursts heard while watching are counted on the run
    let burst = json!({"event_id": "b", "state": "closed", "freq": 457.9375, "ch": 9, "kind": "telemetry",
                       "peak_rssi": 512, "_label": "EOT (end of train)"});
    assert!(!w.consider(&burst, &r.cfg, Some("scan"), false));
    let cur = w.current().unwrap();
    assert_eq!((cur["eot_bursts"].as_i64(), cur["peak_rssi"].as_i64()), (Some(1), Some(512)));
    assert_eq!(r.mon.trigger_margin(), 15.0);
}

// commands -------------------------------------------------------------------------------------------

struct Host {
    mon: Arc<ScannerMonitor>,
    state: PathBuf,
    cfg: Value,
    reports: Mutex<Vec<(String, Value)>>,
}

impl CommandHost for Host {
    fn monitor(&self) -> &ScannerMonitor {
        &self.mon
    }
    fn state_dir(&self) -> PathBuf {
        self.state.clone()
    }
    fn config(&self) -> Value {
        self.cfg.clone()
    }
    fn radio_check(&self, send: bool) -> Result<Value, ScannerError> {
        let data = if self.mon.connected() {
            checks::radio_check_scanner(&self.mon, &self.state, &self.cfg["reference"], &self.timing())?
        } else {
            json!({"freq": 162.55, "rssi": null, "squelch_open": null, "ok": false, "error": "no scanner or SDR connected"})
        };
        if send {
            self.report("radio_check", data.clone());
        }
        Ok(data)
    }
    fn report(&self, rtype: &str, data: Value) {
        self.reports.lock().unwrap().push((rtype.to_string(), data));
    }
}

#[test]
fn commands_need_scanner() {
    let state = tmp("cmdnone");
    let clock = FakeClock::new(1000.0);
    let mon = Arc::new(ScannerMonitor::new("auto", 57600, Hooks { finder: Arc::new(|| None), clock: clock.clock(),
                                                                   ..Hooks::default() }));
    assert!(!mon.try_connect());
    assert_eq!(mon.last_error().as_deref(), Some("no CP2104 cable found"));
    let host = Host { mon, state, cfg: plan("7"), reports: Mutex::new(Vec::new()) };
    for t in ["reception_test", "detection_test", "report_channel_memory", "resume_scan"] {
        assert_eq!(CommandRunner::run(&host, &json!({"id": "c1", "type": t, "args": {}})),
                   json!({"ok": false, "data": null, "error": "no scanner connected"}));
    }
    assert_eq!(CommandRunner::run(&host, &json!({"id": "c2", "type": "set_volume", "args": {"level": 3}}))["error"],
               "no scanner connected");
    let r = CommandRunner::run(&host, &json!({"id": "c3", "type": "radio_check", "args": {}}));
    assert_eq!(r["ok"], false);
    assert!(r["error"].is_string());
    assert_eq!(CommandRunner::run(&host, &json!({"id": "c4", "type": "reboot"}))["error"], "unknown command type 'reboot'");
}

#[test]
fn commands_with_scanner() {
    let r = rig("cmds");
    let host = Host { mon: r.mon.clone(), state: r.state.clone(), cfg: r.cfg.clone(), reports: Mutex::new(Vec::new()) };
    assert_eq!(CommandRunner::run(&host, &json!({"id": "c", "type": "set_volume", "args": {"level": 4}}))["ok"], true);
    assert_eq!(r.fake.lock().volume, 4);
    assert_eq!(r.mon.volume(), Some(4));
    assert_eq!(CommandRunner::run(&host, &json!({"id": "c", "type": "set_squelch", "args": {"level": 99}})),
               json!({"ok": false, "data": null, "error": "level must be 0-15"}));
    let bad = CommandRunner::run(&host, &json!({"id": "c", "type": "set_squelch", "args": {"level": "x"}}));
    assert_eq!(bad["error"], "bad arguments: invalid literal for int() with base 10: 'x'");
    let res = CommandRunner::run(&host, &json!({"id": "c", "type": "report_channel_memory", "args": {}}));
    assert_eq!(res["ok"], true);
    assert_eq!(res["data"]["channels"].as_array().unwrap().len(), 50);
    assert_eq!(host.reports.lock().unwrap().last().unwrap().0, "channel_memory");
    assert_eq!(CommandRunner::run(&host, &json!({"id": "c", "type": "resume_scan"})),
               json!({"ok": true, "data": {}, "error": null}));
}

#[test]
fn on_connect_restores_and_errors_keep_the_scanner() {
    let r = rig("onconnect");
    r.load_plan();
    std::fs::write(r.state.join("scanner_restore.json"),
                   json!({"channels": {"1": {"ch": 1, "freq": 160.905, "delay": "1", "lockout": false}}, "scg": null}).to_string()).unwrap();
    r.fake.program_channel(1, 162.55, false, false);
    r.mon.disconnect("replug");
    let state = r.state.clone();
    let focus = r.focus.clone();
    r.mon.set_hooks(|h| h.on_connect = Some(Arc::new(move |m: &ScannerMonitor| {
        checks::recover_after_crash(m, &state)?;
        focus.recover()
    })));
    assert!(r.mon.try_connect());
    assert_eq!(r.fake.lock().channels[&1].freq, 1609050);
    r.mon.disconnect("replug");
    r.mon.set_hooks(|h| h.on_connect = Some(Arc::new(|_m: &ScannerMonitor| Err(ScannerError::Io("boom".into())))));
    assert!(r.mon.try_connect() && r.mon.connected());
}

#[test]
fn monitor_thread_connects_and_polls() {
    let r = rig("thread");
    r.mon.disconnect("restart");
    // real sleeps for the thread; the fake clock only stamps events
    r.mon.set_hooks(|h| h.sleep = railspotting_scanner::real_sleep());
    let handle = r.mon.start();
    for _ in 0..200 {
        if r.mon.connected() && r.mon.polls() > 3 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(r.mon.connected() && r.mon.polls() > 3);
    r.mon.stop();
    handle.join().unwrap();
}
