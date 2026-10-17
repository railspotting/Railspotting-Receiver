//! tests/data/expected.json records an earlier receiver version driving the monitor, checks and
//! focus against a fake SR30C. The same script here must send the radio the same commands, give
//! the same results and write the same files.
use railspotting_common::{builtin_config, FakeClock};
use railspotting_scanner::checks;
use railspotting_scanner::fake::{CarrierWorld, FakeSR30C};
use railspotting_scanner::focus::{FocusManager, RoleManager};
use railspotting_scanner::monitor::{Hooks, ScannerMonitor};
use railspotting_scanner::{fake_sleep, Timing};
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const NOISY: [&str; 7] = ["rssi", "rssi_median", "peak_rssi", "margin", "vhf_noise", "noise", "detect_s"];

fn strip(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(m.iter().filter(|(k, _)| !NOISY.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), strip(v))).collect()),
        Value::Array(a) => Value::Array(a.iter().map(strip).collect()),
        other => other.clone(),
    }
}

/// Text that quotes a noisy reading (the radio check error, the reception verdict) is left out.
fn drop_keys(v: &Value, keys: &[&str]) -> Value {
    match v {
        Value::Object(m) => Value::Object(m.iter().filter(|(k, _)| !keys.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone())).collect()),
        other => other.clone(),
    }
}

fn plan(version: &str) -> Value {
    let mut cfg = builtin_config();
    cfg["config_version"] = json!(version);
    cfg
}

struct Run {
    fake: FakeSR30C,
    state: PathBuf,
    seen: usize,
    steps: Vec<Value>,
}

impl Run {
    fn step(&mut self, name: &str, result: Value, files: &[&str]) {
        let log: Vec<String> = self.fake.lock().log[self.seen..].to_vec();
        self.seen += log.len();
        let mut out = Map::new();
        out.insert("name".into(), json!(name));
        out.insert("log".into(), json!(log));
        out.insert("result".into(), strip(&result));
        for f in files {
            let p = self.state.join(f);
            out.insert(f.to_string(), std::fs::read_to_string(&p).map(Value::String).unwrap_or(Value::Null));
        }
        self.steps.push(Value::Object(out));
    }
}

#[test]
fn same_script_same_radio_traffic() {
    let want: Value = serde_json::from_str(&std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/expected.json")).unwrap()).unwrap();

    let state = std::env::temp_dir().join(format!("rs-scanner-parity-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&state);
    std::fs::create_dir_all(&state).unwrap();
    let clock = FakeClock::new(1000.0);
    let t = Timing::fake(&clock);
    let fake = FakeSR30C::new(Arc::new(CarrierWorld::default()), clock.clock());
    let events = Arc::new(Mutex::new(Vec::<Value>::new()));
    let ev2 = events.clone();
    let cfg = plan("7");
    let cfg_hook = cfg.clone();
    let mon = Arc::new(ScannerMonitor::new("auto", 57600, Hooks {
        emit: Arc::new(move |e| ev2.lock().unwrap().push(e)),
        get_config: Arc::new(move || cfg_hook.clone()),
        opener: fake.opener(),
        finder: Arc::new(|| Some("/dev/ttyUSB0".into())),
        clock: clock.clock(),
        sleep: fake_sleep(&clock),
        on_connect: None,
    }));
    let focus = Arc::new(FocusManager::new(mon.clone(), &state, clock.clock()));
    let mut run = Run { fake: fake.clone(), state: state.clone(), seen: 0, steps: Vec::new() };

    assert!(mon.try_connect());
    run.step("connect", mon.heartbeat_info(), &[]);

    fake.program_channel(1, 160.905, true, false);
    fake.program_channel(9, 457.9375, true, false);
    fake.program_channel(3, 161.19, true, true);
    let r = checks::plan_sync(&mon, &cfg, &state).unwrap();
    run.step("plan_sync 7", json!(r), &["scanner_plan.json"]);

    fake.lock().banks = "1111110110".into();
    fake.program_channel(2, 160.5, true, false);
    run.step("scan_setup", checks::scan_setup_check(&mon, &cfg).unwrap(), &[]);
    fake.program_channel(2, 160.245, true, false);

    run.step("detection_test fail", checks::detection_test(&mon, &state, &cfg, 1.0, &t).unwrap(), &[]);
    run.step("radio_check weak", checks::radio_check_scanner_for(&mon, &state, &cfg["reference"], 0.5, &t).unwrap(), &[]);
    run.step("reception_test", checks::reception_test(&mon, &state, &cfg, 0.25, &t).unwrap(), &[]);

    focus.start(452.9375, Some(30.0), "auto_eot", false, Some("HOT (head of train)")).unwrap();
    run.step("focus start", json!(focus.active()), &["focus.json"]);
    focus.end("time up").unwrap();
    run.step("focus end", json!(focus.active()), &["focus.json"]);

    std::fs::write(state.join("scanner_restore.json"), json!(
        {"channels": {"1": {"ch": 1, "freq": 160.905, "delay": "1", "lockout": false},
                      "50": {"ch": 50, "freq": null, "delay": "0", "lockout": true}}, "scg": "0111110110"}).to_string()).unwrap();
    fake.program_channel(1, 162.55, false, false);
    run.step("recover_after_crash", json!(checks::recover_after_crash(&mon, &state).unwrap()), &["scanner_restore.json"]);

    run.step("channel_memory", checks::channel_memory(&mon).unwrap(), &[]);

    let mut cfg8 = plan("8");
    let chans: Vec<Value> = cfg8["channels"].as_array().unwrap().iter().filter(|c| c["ch"] != 4).cloned().collect();
    cfg8["channels"] = json!(chans);
    cfg8["channels"][0]["freq"] = json!(160.920);
    run.step("plan_sync 8", json!(checks::plan_sync(&mon, &cfg8, &state).unwrap()), &["scanner_plan.json"]);

    let mut roles = RoleManager::new(Some(focus.clone()), clock.clock());
    roles.tick("eot", "voice is on an SDR; EOT is the priority", &cfg8, false).unwrap();
    clock.advance(16.0);
    run.step("role eot", json!(roles.tick("eot", "x", &cfg8, false).unwrap()), &[]);

    for _ in 0..5 {
        mon.poll_once().unwrap();
        clock.advance(0.03);
    }
    run.step("polls parked", json!({"state": mon.state(), "freq": mon.freq(), "events": events.lock().unwrap().len()}), &[]);

    mon.borrow("set_volume", |s| s.set_volume(4)).unwrap();
    let vol = fake.lock().volume;
    run.step("set_volume", json!(vol), &[]);

    fake.set_connected(false);
    let err = mon.poll_once().unwrap_err().to_string();
    mon.disconnect(&err);
    run.step("disconnect", json!({"error": err, "info": mon.heartbeat_info()}), &[]);

    let wsteps = want["steps"].as_array().unwrap();
    assert_eq!(run.steps.len(), wsteps.len());
    for (got, want) in run.steps.iter().zip(wsteps) {
        let name = want["name"].as_str().unwrap();
        assert_eq!(got["log"], want["log"], "commands sent in step {name}");
        let (mut g, mut w) = (got.clone(), want.clone());
        let noisy_text: &[&str] = match name {
            "radio_check weak" => &["error"],
            "reception_test" => &["verdict", "detail"],
            _ => &[],
        };
        g["result"] = drop_keys(&g["result"], noisy_text);
        w["result"] = drop_keys(&w["result"], noisy_text);
        assert_eq!(g, w, "step {name}");
    }

    let f = fake.lock();
    assert_eq!(json!(f.banks), want["final"]["banks"]);
    let chans: Map<String, Value> = f.channels.iter().filter(|(_, c)| c.freq != 0 || c.lockout != "1")
        .map(|(n, c)| (n.to_string(), json!({"freq": c.freq, "delay": c.delay, "lockout": c.lockout}))).collect();
    assert_eq!(Value::Object(chans), want["final"]["channels"]);
    let _ = std::fs::remove_dir_all(&state);
}
