//! End to end: `run --simulate` against the fake headend (real time).

use crate::common::*;
use railspotting_engine::AgentOptions;
use serde_json::json;
use std::time::{Duration, Instant};

fn wait_for(secs: f64, f: &mut dyn FnMut() -> bool) -> bool {
    let end = Instant::now() + Duration::from_secs_f64(secs);
    while Instant::now() < end {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    f()
}

#[test]
fn simulate_uploads_signed_events_to_the_headend() {
    let t = Tmp::new("e2e-sim");
    let c = clock();
    let h = headend(&c); // the headend's clock only stamps replies; signing uses real time below
    h.state().clock = railspotting_common::system_clock();
    let agent = make_agent_with(t.path(), Some(&h), None, json!({"sdr": {"devices": [], "auto_assign": true}}), true,
                                AgentOptions { simulate: true, clock: Some(railspotting_common::system_clock()), ..Default::default() });
    // a strong call on the CSX road channel a moment after the dongles open
    let world = agent.world.clone().unwrap();
    let now = railspotting_common::now();
    world.carrier(161.10, 25.0, now + 2.0, Some(now + 4.0), 1000.0, 2500.0, "voice");
    agent.start();
    let mut got = vec![];
    let ok = wait_for(120.0, &mut || {
        got = h.state().event_log.clone();
        got.iter().any(|e| e["state"] == "closed" && e["ch"] == 7) && !h.state().heartbeats.is_empty()
    });
    let scanner = agent.monitor.connected();
    let caps_now = agent.capabilities();
    agent.stop();
    agent.join(5.0);
    for s in agent.sdrs() {
        s.join();
    }
    assert!(ok, "events seen by the headend: {got:?}");
    let st = h.state();
    // every request was signed by the stored key (the fake headend checks each signature and nonce)
    assert!(st.devices.contains_key(DEVICE_ID));
    let closed = got.iter().find(|e| e["state"] == "closed" && e["ch"] == 7).unwrap();
    assert!(closed["freq"] == 161.1 && closed["kind"] == "voice");
    let open = got.iter().find(|e| e["event_id"] == closed["event_id"] && e["state"] == "open");
    assert!(open.is_some(), "the open state was sent first");
    let hb = st.heartbeats.last().unwrap();
    assert_eq!(hb["agent_version"], railspotting_common::AGENT_VERSION);
    let caps = &hb["capabilities"];
    // the scanner and three dongles: EOT, ITC and VHF (the first three roles)
    let receivers = caps["receivers"].as_array().unwrap();
    assert_eq!(receivers.len(), 4);
    assert!(receivers.iter().any(|r| r["role"] == "itc"), "{receivers:?}");
    assert!(scanner && caps_now["receivers"][0]["product"] == "SR30C"); // the simulated scanner
    assert_eq!(caps_now["features"]["simultaneous_voice"], json!(true));
    assert!(!hb.to_string().contains("SIM0000"), "dongle serials are never sent");
    let status = read_json(&agent.state_dir.join("status.json"));
    assert_eq!(status["simulate"], json!(true));
}
