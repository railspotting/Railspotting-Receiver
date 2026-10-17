//! A fake headend on 127.0.0.1 for trying `run --simulate` by hand. Approves every link at
//! once and prints what it received every 10 s.
//!
//!     cargo run -p railspotting-receiver --example fake_headend -- [port-file] [seconds]

use railspotting_proto::fake::FakeHeadend;
use serde_json::json;
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut cfg = railspotting_common::builtin_config();
    cfg["config_version"] = json!("7");
    let h = FakeHeadend::start(railspotting_common::system_clock(), cfg).expect("fake headend starts");
    println!("fake headend on {}", h.url());
    if let Some(p) = args.get(1) {
        let _ = std::fs::write(p, h.url());
    }
    let secs: f64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(3600.0);
    let end = Instant::now() + Duration::from_secs_f64(secs);
    let mut n = 0;
    while Instant::now() < end {
        std::thread::sleep(Duration::from_secs(1));
        let codes: Vec<String> = h.state().links.values().filter(|l| l.status == "pending").map(|l| l.code.clone()).collect();
        for c in codes {
            println!("approving link {c}: {:?}", h.approve_link(&c, None));
        }
        n += 1;
        if n % 10 == 0 {
            let st = h.state();
            let closed = st.event_log.iter().filter(|e| e["state"] == "closed").count();
            println!("events {} ({} closed), reports {}, heartbeats {}, devices {}", st.event_log.len(), closed,
                     st.reports.len(), st.heartbeats.len(), st.devices.len());
        }
    }
    let st = h.state();
    for e in &st.event_log {
        println!("event {} {} ch {} {} peak {}", e["state"], e["freq"], e["ch"], e["kind"], e["peak_rssi"]);
    }
    for r in &st.reports {
        println!("report {}", r["type"]);
    }
    if let Some(hb) = st.heartbeats.last() {
        println!("last heartbeat {hb}");
    }
}
