//! ITC upload (contract v1.18): wayside state changes and base stations go to the headend only
//! while its /config says `itc.upload: true`; repeats never do.

use crate::common::*;
use railspotting_engine::agent::{AgentOptions, ITC_NAMESPACE};
use railspotting_engine::logging::Capture;
use serde_json::{json, Value};

fn wiu(t: f64, unit: &str, codes: &[i64]) -> Value {
    json!({"type": "WIU", "wiu": unit, "t": t, "ch": 114, "codes": codes, "indicators": "b600", "crc_ok": true,
           "level_db": 20.0, "floor_db": -80.0})
}

fn beacon(t: f64) -> Value {
    json!({"type": "BEACON", "radio": "123456", "t": t, "ch": 102, "lat": 40.5, "lon": -90.25, "channel": 114,
           "channels": [101, 127], "util": 37, "level_db": 18.8, "floor_db": -80.0, "crc_ok": true})
}

/// What the ITC dongle decoded.
fn hear(agent: &railspotting_engine::Agent, msgs: &[Value]) {
    let s = agent.sdrs().into_iter().find(|s| s.with(|x| x.role == "itc")).expect("an ITC dongle");
    s.with(|x| {
        x.connected = true;
        let t0 = msgs[0]["t"].as_f64().unwrap();
        x.itc.as_mut().unwrap().handle(msgs, t0, 220.3, 1.024e6)
    });
}

fn upload_on(on: bool) -> Value {
    json!({"upload": on, "max_batch": 200, "flush_interval_s": 10, "max_age_s": 604800})
}

#[test]
fn itc_goes_to_the_headend_only_when_it_says_so() {
    let (c, t) = (clock(), Tmp::new("itc-up"));
    let h = headend(&c);
    let log = Capture::start();
    let agent = make_agent_with(t.path(), Some(&h), Some(&c), json!({"sdr": {"devices": [], "auto_assign": true}}), true,
                                AgentOptions::default());
    agent.approved.set();
    let now = c.get();

    // a headend without v1.18: nothing queued, nothing sent, but the heartbeat says ITC is alive
    hear(&agent, &[wiu(now, "1234560ff0", &[3, 15]), beacon(now + 1.0)]);
    assert_eq!(agent.itc_outbox.depth().unwrap(), 0);
    assert!(!agent.itc_upload_once().unwrap());
    let hb = agent.heartbeat_body();
    assert_eq!(hb["itc"]["units_heard"], 1);
    assert_eq!(hb["itc"]["messages"]["WIU"], 1);
    assert_eq!(hb["itc"]["bases"][0]["base"], "123456");
    assert_eq!(hb["itc"]["outbox"], 0);
    assert_eq!(agent.status()["itc_upload"], json!({"on": false, "outbox": 0}));

    // the owner turns it on: a starting state for each unit heard, then only changes
    let mut cfg = agent.config();
    cfg["itc"] = upload_on(true);
    cfg["config_version"] = json!("8");
    h.state().config = cfg.clone();
    agent.apply_config(cfg, None);
    assert!(log.text().contains("ITC upload is on"));
    hear(&agent, &[wiu(now + 4.0, "1234560ff0", &[3, 15]), wiu(now + 8.0, "1234560ff0", &[3, 15]),
                   wiu(now + 12.0, "1234560ff0", &[8, 15]), beacon(now + 13.0)]);
    assert_eq!(agent.itc_outbox.depth().unwrap(), 3);
    assert!(agent.itc_upload_once().unwrap());
    assert_eq!(agent.itc_outbox.depth().unwrap(), 0);
    let got = h.state().itc_log.clone();
    assert_eq!(got.len(), 3);
    assert_eq!((got[0]["type"].as_str(), got[0]["first"].as_bool()), (Some("wayside"), Some(true)));
    assert_eq!(got[1]["previous_codes"], json!([3, 15]));
    assert_eq!(got[1]["codes"], json!([8, 15]));
    assert_eq!(got[2]["type"], "base");
    let ms = ((now + 12.0) * 1000.0).round() as i64;
    let want = uuid::Uuid::new_v5(&ITC_NAMESPACE, format!("itc-wayside:1234560ff0:{ms}:8,15").as_bytes());
    assert_eq!(got[1]["id"], json!(want.to_string()));
    let text = serde_json::to_string(&got).unwrap();
    assert!(!text.contains("group") && !text.contains("changed_fields"), "{text}");

    // a retry of the same record is a duplicate, and a refused one is dropped
    agent.queue_itc(&got[1]);
    let mut bad = got[1].clone();
    bad["id"] = json!("bad");
    bad["codes"] = json!([99]);
    agent.queue_itc(&bad);
    assert!(agent.itc_upload_once().unwrap());
    assert_eq!(agent.itc_outbox.depth().unwrap(), 0);
    assert!(log.text().contains("headend refused ITC record bad"));
    assert_eq!(h.state().itc.len(), 3);

    // records older than max_age_s are not sent
    agent.queue_itc(&json!({"id": "old", "type": "wayside", "t": now - 8.0 * 86400.0}));
    assert!(!agent.itc_upload_once().unwrap());
    assert_eq!(agent.itc_outbox.depth().unwrap(), 0);

    // an admin turns it off on the headend: 403 itc_off, the queue is dropped, nothing more goes
    h.state().config["itc"] = upload_on(false);
    h.state().config["config_version"] = json!("9");
    hear(&agent, &[wiu(now + 20.0, "1234560ff0", &[3, 15])]);
    assert_eq!(agent.itc_outbox.depth().unwrap(), 1);
    assert!(!agent.itc_upload_once().unwrap());
    assert!(!agent.itc_upload.is_on());
    assert_eq!(agent.itc_outbox.depth().unwrap(), 0);
    assert!(log.text().contains("the headend says ITC upload is off"));
    hear(&agent, &[wiu(now + 24.0, "1234560ff0", &[8, 15])]);
    assert_eq!(agent.itc_outbox.depth().unwrap(), 0);
    assert_eq!(agent.config()["config_version"], "9", "it fetched /config again");
    assert_eq!(h.state().itc.len(), 3);
}

#[test]
fn a_restart_with_upload_off_drops_what_was_queued() {
    let (c, t) = (clock(), Tmp::new("itc-restart"));
    let h = headend(&c);
    let over = json!({"sdr": {"devices": [], "auto_assign": true}});
    let agent = make_agent_with(t.path(), Some(&h), Some(&c), over.clone(), true, AgentOptions::default());
    let mut cfg = agent.config();
    cfg["itc"] = upload_on(true);
    agent.apply_config(cfg, None);
    hear(&agent, &[wiu(c.get(), "1234560ff0", &[3])]);
    assert_eq!(agent.itc_outbox.depth().unwrap(), 1);
    agent.itc_outbox.close();
    drop(agent);
    // the saved /config still says on: the queue is kept
    let again = make_agent_with(t.path(), Some(&h), Some(&c), over.clone(), true, AgentOptions::default());
    assert!(again.itc_upload.is_on());
    assert_eq!(again.itc_outbox.depth().unwrap(), 1);
    let mut cfg = again.config();
    cfg["itc"] = upload_on(false);
    again.apply_config(cfg, None);
    assert_eq!(again.itc_outbox.depth().unwrap(), 0);
}
