#![allow(dead_code, clippy::type_complexity)]
//! Shared test helpers: a fake clock, the plan, a running fake
//! headend, an agent with a stored device ID, and SDR sources fed from a simulated world.

use railspotting_common::{builtin_config, FakeClock, Settings};
use railspotting_dsp::sdr::{Emit, SdrSource};
use railspotting_dsp::sim::World;
use railspotting_proto::fake::FakeHeadend;
use railspotting_proto::keys;
use railspotting_engine::{Agent, AgentOptions};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub const DEVICE_ID: &str = "rs-b827eb1a2b3c";
pub const MAC: &str = "b8:27:eb:1a:2b:3c";

pub fn vhf() -> Value {
    json!({"serial": "30002222", "role": "vhf", "sample_rate": 2_880_000.0, "gain": 20.7, "name": "VHF SDR"})
}

pub fn eot() -> Value {
    json!({"serial": "30001111", "role": "eot", "sample_rate": 2_400_000.0, "gain": 40.2, "name": "UHF SDR"})
}

pub fn clock() -> FakeClock {
    FakeClock::new(1_790_865_400.0)
}

pub fn plan(version: &str) -> Value {
    let mut cfg = builtin_config();
    cfg["config_version"] = json!(version);
    cfg
}

pub fn channels(cfg: &Value) -> Vec<Value> {
    cfg["channels"].as_array().cloned().unwrap_or_default()
}

pub fn headend(clock: &FakeClock) -> FakeHeadend {
    FakeHeadend::start(clock.clock(), plan("7")).expect("fake headend starts")
}

pub fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

/// A temporary folder, removed when dropped.
pub struct Tmp(pub PathBuf);

impl Tmp {
    pub fn new(name: &str) -> Tmp {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let d = std::env::temp_dir().join(format!("rs-recv-{name}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Tmp(d)
    }
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn settings(tmp: &Path, headend: Option<&FakeHeadend>, over: Value) -> Settings {
    let mut raw = json!({"transcription": false, "web": {"enabled": false}, "sdr": {"devices": []}});
    if let Some(h) = headend {
        raw["server_url"] = json!(h.url());
    }
    if let (Some(r), Value::Object(o)) = (raw.as_object_mut(), over) {
        for (k, v) in o {
            r.insert(k, v);
        }
    }
    Settings::from_value(&raw, tmp)
}

/// An agent with a stored device ID that the headend knows (linked), or none yet.
pub fn make_agent_with(tmp: &Path, headend: Option<&FakeHeadend>, clock: Option<&FakeClock>, over: Value, linked: bool,
                       mut opts: AgentOptions) -> Arc<Agent> {
    railspotting_engine::logging::setup_for_tests();
    let state = tmp.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let ident = state.join("identity.json");
    if !ident.exists() {
        std::fs::write(&ident, json!({"device_id": if linked { json!(DEVICE_ID) } else { Value::Null },
                                      "mac": MAC, "interface": "eth0"}).to_string()).unwrap();
    }
    let st = settings(tmp, headend, over);
    if opts.clock.is_none() {
        opts.clock = Some(clock.cloned().unwrap_or_else(self::clock).clock());
    }
    let agent = Agent::new(st, opts).expect("agent");
    if let (Some(h), Some(id)) = (headend, agent.device_id()) {
        if !h.state().devices.contains_key(&id) {
            h.add_device(&id, &keys::public_bytes(&agent.key), "approved");
        }
    }
    agent
}

pub fn make_agent(tmp: &Path, headend: &FakeHeadend, clock: &FakeClock) -> Arc<Agent> {
    make_agent_with(tmp, Some(headend), Some(clock), json!({}), true, AgentOptions::default())
}

pub fn unlinked(tmp: &Path, headend: &FakeHeadend, clock: &FakeClock) -> Arc<Agent> {
    make_agent_with(tmp, Some(headend), Some(clock), json!({}), false, AgentOptions::default())
}

/// Collected events and the emit that collects them.
pub fn sink() -> (Arc<Mutex<Vec<Map<String, Value>>>>, Emit) {
    let ev = Arc::new(Mutex::new(vec![]));
    let e2 = ev.clone();
    (ev, Arc::new(move |e| e2.lock().unwrap().push(e)))
}

/// Feed blocks of world IQ from t_start to t_end (simulated seconds).
pub fn run(src: &mut SdrSource, world: &World, t_start: f64, t_end: f64) -> f64 {
    let mut t = t_start;
    let dur = src.block as f64 / src.rate;
    while t < t_end {
        let iq = world.iq(src.center.unwrap_or(0.0), src.rate, t, src.block);
        src.process_block(&iq, t + dur);
        t += dur;
    }
    t
}

pub fn read_json(p: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

pub fn reports(agent: &Agent, kind: &str) -> Vec<Value> {
    agent.outbox.next_reports(100).unwrap().into_iter().map(|(_, r)| r).filter(|r| r["type"] == kind).collect()
}
