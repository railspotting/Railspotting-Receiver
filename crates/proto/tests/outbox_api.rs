//! The API side of the outbox: retries, idempotency, ordering, oversize batches,
//! commands and config refetch, at the client level (the outbox itself is in the store crate).

mod common;

use common::*;
use serde_json::{json, Value};

fn event(state: &str, ts: f64) -> Value {
    json!({"event_id": uuid::Uuid::new_v4().to_string(), "state": state, "start_ts": ts,
           "end_ts": if state == "open" { Value::Null } else { json!(ts + 1.0) }, "freq": 160.95, "ch": 5,
           "kind": "voice", "peak_rssi": 420, "avg_rssi": 410, "samples": 3})
}

#[test]
fn retry_and_idempotency() {
    let c = clock();
    let h = headend(&c);
    let api = approved_client(&h, &c);
    let e = event("open", 1.0);
    h.set_down(true);
    assert!(api.post_events(std::slice::from_ref(&e)).unwrap_err().network());
    h.set_down(false);
    let r = api.post_events(std::slice::from_ref(&e)).unwrap();
    assert_eq!(r, json!({"accepted": [e["event_id"]], "duplicates": []}));
    // same event again: the server says duplicate
    let r = api.post_events(std::slice::from_ref(&e)).unwrap();
    assert_eq!(r, json!({"accepted": [], "duplicates": [e["event_id"]]}));
    // an update after closed is a duplicate too
    let mut closed = e.clone();
    closed["state"] = json!("closed");
    closed["end_ts"] = json!(2.0);
    assert_eq!(api.post_events(&[closed.clone()]).unwrap()["accepted"], json!([e["event_id"]]));
    let mut late = closed.clone();
    late["samples"] = json!(9);
    assert_eq!(api.post_events(&[late]).unwrap()["duplicates"], json!([e["event_id"]]));
    assert_eq!(h.state().events[e["event_id"].as_str().unwrap()]["state"], "closed");
}

#[test]
fn catch_up_in_order() {
    let c = clock();
    let h = headend(&c);
    let api = approved_client(&h, &c);
    let mut batch = vec![];
    let mut sent = vec![];
    for i in 0..30 {
        let e = event("open", i as f64);
        let mut cl = e.clone();
        cl["state"] = json!("closed");
        cl["end_ts"] = json!(i as f64 + 0.5);
        sent.push(e["event_id"].clone());
        batch.push(e);
        batch.push(cl);
    }
    for chunk in batch.chunks(25) {
        api.post_events(chunk).unwrap();
    }
    api.post_report(&json!({"type": "radio_check", "ts": c.get(),
                            "data": {"freq": 162.55, "rssi": 461, "squelch_open": 0.8, "ok": true, "error": null}}))
        .unwrap();
    let st = h.state();
    let seen: Vec<Value> = st.event_log.iter().filter(|e| e["state"] == "open").map(|e| e["event_id"].clone()).collect();
    assert_eq!(seen, sent);
    assert!(sent.iter().all(|id| st.events[id.as_str().unwrap()]["state"] == "closed"));
    assert_eq!(st.reports.last().unwrap()["type"], "radio_check");
}

#[test]
fn oversize_batch_error() {
    let c = clock();
    let h = headend(&c);
    let api = approved_client(&h, &c);
    h.fail_next(413, "too_large", &[]);
    let e = api.post_events(&[event("open", 1.0), event("open", 2.0)]).unwrap_err();
    assert_eq!((e.status, e.error.as_str()), (Some(413), "too_large"));
    assert!(api.post_events(&[event("open", 1.0)]).is_ok());
}

#[test]
fn conflict_409_is_returned_not_raised() {
    let c = clock();
    let h = headend(&c);
    let api = approved_client(&h, &c);
    h.fail_next(409, "conflict", &[]);
    let r = api.heartbeat(&json!({"ts": 1})).unwrap();
    assert_eq!(r, json!({"error": "conflict", "message": "injected"}));
}

#[test]
fn commands_fetched_and_results_posted() {
    let c = clock();
    let h = headend(&c);
    let api = approved_client(&h, &c);
    h.state().commands.push(json!({"id": "c_91", "type": "detection_test", "args": {}, "issued_ts": 1.0}));
    let hb = api.heartbeat(&json!({"ts": c.get()})).unwrap();
    assert_eq!(hb["commands_pending"], 1);
    assert_eq!(hb["server_time"], c.get());
    let cmds = api.get_commands().unwrap();
    assert_eq!(cmds.len(), 1);
    assert!(api.get_commands().unwrap().is_empty());
    let result = json!({"ok": false, "data": null, "error": "no scanner connected"});
    assert_eq!(api.post_command_result("c_91", &result).unwrap(), json!({"stored": true}));
    assert_eq!(h.state().results["c_91"], result);
}

#[test]
fn config_change_is_refetched() {
    let c = clock();
    let h = headend(&c);
    let api = approved_client(&h, &c);
    let (_, etag) = api.get_config(None).unwrap();
    let mut cfg = plan("8");
    cfg["channels"][0]["freq"] = json!(158.0);
    h.state().config = cfg;
    assert_eq!(api.heartbeat(&json!({})).unwrap()["config_version"], "8");
    let (cfg, etag2) = api.get_config(etag.as_deref()).unwrap();
    let cfg = cfg.expect("a new version is sent in full");
    assert_eq!(cfg["config_version"], "8");
    assert_eq!(cfg["channels"][0]["freq"], 158.0);
    assert_eq!(etag2.as_deref(), Some("\"8\""));
    assert_eq!(api.get_config(etag2.as_deref()).unwrap().0, None);
}

#[test]
fn transcript_round_trips_exactly() {
    let c = clock();
    let h = headend(&c);
    let api = approved_client(&h, &c);
    let mut e = event("closed", 1.0);
    e["transcript"] = json!({"text": "Clear signal, over.", "engine": "whisper-base.en",
                             "confidence": 0.8, "language": "en", "duration_s": 2.4});
    api.post_events(&[e.clone()]).unwrap();
    assert_eq!(h.state().events[e["event_id"].as_str().unwrap()]["transcript"], e["transcript"]);
}

#[test]
fn replayed_nonce_is_refused() {
    use railspotting_proto::keys;
    let c = clock();
    let h = headend(&c);
    let api = approved_client(&h, &c);
    let path = "/api/v1/commands";
    let hdrs = keys::signed_headers(&test_key(), Some(DEVICE_ID), "GET", path, b"", c.get() as i64,
                                    Some("00112233445566778899aabbccddeeff"));
    let ok = api.request("GET", "/commands", None, false, &hdrs, "", true).unwrap();
    assert_eq!(ok.status, 200);
    let e = api.request("GET", "/commands", None, false, &hdrs, "", true).unwrap_err();
    assert_eq!((e.status, e.error.as_str()), (Some(401), "replay"));
}

#[test]
fn idle_connection_is_kept_between_heartbeats() {
    use std::io::{BufRead, BufReader, Write};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    // a server that keeps connections open, counting how many it was asked for
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let accepted = Arc::new(AtomicUsize::new(0));
    let n = accepted.clone();
    std::thread::spawn(move || {
        for s in listener.incoming().flatten() {
            n.fetch_add(1, Ordering::SeqCst);
            std::thread::spawn(move || {
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut w = s;
                loop {
                    let mut line = String::new();
                    // read one request head (these GETs have no body)
                    loop {
                        line.clear();
                        if r.read_line(&mut line).unwrap_or(0) == 0 {
                            return;
                        }
                        if line == "\r\n" {
                            break;
                        }
                    }
                    if w.write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}").is_err() {
                        return;
                    }
                }
            });
        }
    });
    let c = clock();
    let api = railspotting_proto::api::ApiClient::new(&format!("http://127.0.0.1:{port}"), None, test_key(), None, c.clock(), 5.0).unwrap();
    assert_eq!(api.keep_idle(), std::time::Duration::from_secs(railspotting_proto::api::KEEP_IDLE_S));
    // longer than the 60 s heartbeat, shorter than nginx's 75 s keepalive_timeout
    const { assert!(railspotting_proto::api::KEEP_IDLE_S > 60 && railspotting_proto::api::KEEP_IDLE_S < 75) };
    api.probe_clock().unwrap();
    // longer than ureq's default of 15 s, which made every 60 s heartbeat a new connection
    std::thread::sleep(std::time::Duration::from_secs(16));
    api.probe_clock().unwrap();
    assert_eq!(accepted.load(Ordering::SeqCst), 1, "the idle connection was not reused");
}
