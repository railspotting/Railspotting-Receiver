//! The engine handle: start, subscribe, settings over the page, stop.

use crate::common::*;
use railspotting_common::Settings;
use railspotting_engine::agent::AgentOptions;
use railspotting_engine::handle::{Engine, EngineEvent, EngineOptions};
use railspotting_engine::web::TOKEN_HEADER;
use railspotting_proto::keys;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

/// (status, body) of one request with optional token and body.
fn request(port: u16, method: &str, path: &str, token: Option<&str>, body: &str) -> (u16, Vec<u8>) {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let tok = token.map(|t| format!("{TOKEN_HEADER}: {t}\r\n")).unwrap_or_default();
    write!(s, "{method} {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n{tok}Content-Type: application/json\r\n\
               Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
    let mut buf = vec![];
    s.read_to_end(&mut buf).unwrap();
    let split = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&buf[..split]).to_string();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    assert!(!head.to_ascii_lowercase().contains("transfer-encoding: chunked"));
    (status, buf[split + 4..].to_vec())
}

fn json_of(b: &[u8]) -> Value {
    serde_json::from_slice(b).unwrap()
}

/// A linked, simulated receiver whose settings come from a receiver.json file.
fn start(tmp: &Path, h: &railspotting_proto::fake::FakeHeadend, token: Option<&str>) -> Engine {
    railspotting_engine::logging::setup_for_tests();
    let state = tmp.join("state");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(state.join("identity.json"),
                   json!({"device_id": DEVICE_ID, "mac": MAC, "interface": "eth0"}).to_string()).unwrap();
    let file = json!({"server_url": h.url(), "transcription": false, "label": "before",
                      "web": {"enabled": true, "bind": "127.0.0.1", "port": 0},
                      "sdr": {"devices": [vhf()]}, "watch_clips": []});
    let path = tmp.join("receiver.json");
    std::fs::write(&path, serde_json::to_string_pretty(&file).unwrap()).unwrap();
    let settings = Settings::load(&path).unwrap();
    let opts = EngineOptions {
        agent: AgentOptions { simulate: true, ..Default::default() },
        token: token.map(str::to_string),
    };
    let engine = Engine::start(settings, opts).expect("engine starts");
    let agent = engine.agent().unwrap();
    h.add_device(DEVICE_ID, &keys::public_bytes(&agent.key), "approved");
    engine
}

#[test]
fn settings_round_trip_over_the_page() {
    let (c, t) = (clock(), Tmp::new("handle1"));
    let h = headend(&c);
    let engine = start(t.path(), &h, Some(TOKEN));
    let port = engine.web_port().expect("page runs");
    let rx = engine.subscribe();
    let first = engine.agent().unwrap();

    // refused without the token, and the GETs that need none still work
    assert_eq!(request(port, "GET", "/api/settings", None, "").0, 403);
    assert_eq!(request(port, "POST", "/api/settings", Some("wrong"), r#"{"label": "x"}"#).0, 403);
    assert_eq!(request(port, "GET", "/api/status", None, "").0, 200);

    let (st, body) = request(port, "GET", "/api/settings", Some(TOKEN), "");
    assert_eq!(st, 200);
    assert_eq!(json_of(&body)["label"], "before");

    // a bad change is refused and nothing is saved
    let (st, body) = request(port, "POST", "/api/settings", Some(TOKEN), r#"{"server_url": "https://example.com"}"#);
    assert_eq!(st, 400);
    assert!(json_of(&body)["error"].as_str().unwrap().contains("server_url"));
    assert_eq!(read_json(&t.path().join("receiver.json"))["label"], "before");

    // a good one is saved, applied and survives on the same port
    let (st, body) = request(port, "POST", "/api/settings", Some(TOKEN),
                             r#"{"label": "Shed", "transcribe_other_calls": true, "watch_clips": [160.95]}"#);
    assert_eq!(st, 200, "{}", String::from_utf8_lossy(&body));
    assert_eq!(json_of(&body)["label"], "Shed");
    let saved = read_json(&t.path().join("receiver.json"));
    assert_eq!(saved["label"], "Shed");
    assert_eq!(saved["transcribe_other_calls"], true);
    assert_eq!(saved["watch_clips"], json!([160.95]));
    assert_eq!(saved["server_url"], json!(h.url())); // the rest of the file is kept
    assert_eq!(saved["sdr"]["devices"], json!([vhf()]));
    let second = engine.agent().unwrap();
    assert!(!std::sync::Arc::ptr_eq(&first, &second), "the agent restarted");
    assert_eq!(second.settings.str("label"), Some("Shed"));
    assert_eq!(second.settings.get("web")["enabled"], false); // the engine serves the page
    assert_eq!(engine.settings().str("label"), Some("Shed"));
    assert_eq!(engine.web_port(), Some(port));
    assert_eq!(json_of(&request(port, "GET", "/api/settings", Some(TOKEN), "").1)["label"], "Shed");
    assert_eq!(request(port, "GET", "/api/status", None, "").0, 200);

    // dongle roles change too
    let devs = json!({"sdr": {"devices": [{"role": "vhf", "serial": "30002222", "gain": 30.0}]}});
    assert_eq!(engine.apply_settings(devs).unwrap()["sdr"]["devices"][0]["gain"], 30.0);
    assert_eq!(read_json(&t.path().join("receiver.json"))["sdr"]["devices"][0]["gain"], 30.0);

    // a linked receiver is not linked again
    let (st, body) = request(port, "POST", "/api/link", Some(TOKEN), "");
    assert_eq!(st, 400);
    assert!(json_of(&body)["error"].as_str().unwrap().contains("already linked"));

    // subscribers heard the status
    let got: Vec<EngineEvent> = rx.try_iter().collect();
    assert!(got.iter().any(|e| matches!(e, EngineEvent::Status(s) if s["device_id"] == DEVICE_ID)), "{got:?}");

    engine.stop();
    assert_eq!(request_or_refused(port), None, "the page stops with the engine");
}

fn request_or_refused(port: u16) -> Option<u16> {
    std::net::TcpStream::connect(("127.0.0.1", port)).ok().map(|_| 0)
}

#[test]
fn receiver_json_is_edited_whole_and_put_back_when_it_fails() {
    let (c, t) = (clock(), Tmp::new("handle4"));
    let h = headend(&c);
    let engine = start(t.path(), &h, Some(TOKEN));
    let port = engine.web_port().unwrap();
    let path = t.path().join("receiver.json");
    assert_eq!(request(port, "GET", "/api/config-file", None, "").0, 403);

    let (st, body) = request(port, "GET", "/api/config-file", Some(TOKEN), "");
    assert_eq!(st, 200);
    let text = json_of(&body)["text"].as_str().unwrap().to_string();
    assert_eq!(serde_json::from_str::<Value>(&text).unwrap()["label"], "before");

    // not JSON, or not an object: refused and the file is untouched
    for bad in ["{\"label\": ", "[1, 2]"] {
        let (st, _) = request(port, "POST", "/api/config-file", Some(TOKEN), &json!({"text": bad}).to_string());
        assert_eq!(st, 400, "{bad}");
    }
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text);

    // a good file is saved as written and the receiver restarts on it
    let mut v: Value = serde_json::from_str(&text).unwrap();
    v["label"] = json!("Whole");
    v["sdr"]["role_order"] = json!(["vhf", "eot"]);
    let new = serde_json::to_string_pretty(&v).unwrap() + "\n";
    let (st, body) = request(port, "POST", "/api/config-file", Some(TOKEN), &json!({"text": new}).to_string());
    assert_eq!(st, 200, "{}", String::from_utf8_lossy(&body));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), new);
    assert_eq!(engine.settings().str("label"), Some("Whole"));
    let s = json_of(&request(port, "GET", "/api/settings", Some(TOKEN), "").1);
    assert_eq!(s["sdr"]["role_order"], json!(["vhf", "eot"]));

    // a file the receiver won't start with: the old one goes back
    let mut broken = v.clone();
    broken["state_dir"] = json!(t.path().join("receiver.json").join("state"));
    let (st, _) = request(port, "POST", "/api/config-file", Some(TOKEN), &json!({"text": broken.to_string()}).to_string());
    assert_eq!(st, 400);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), new);
    assert!(engine.agent().is_some());
    assert_eq!(engine.settings().str("label"), Some("Whole"));
    engine.stop();
}

#[test]
fn without_a_token_the_page_stays_read_only() {
    let (c, t) = (clock(), Tmp::new("handle2"));
    let h = headend(&c);
    let engine = start(t.path(), &h, None);
    let port = engine.web_port().unwrap();
    assert_eq!(request(port, "POST", "/api/settings", Some(TOKEN), r#"{"label": "x"}"#).0, 501);
    assert_eq!(request(port, "POST", "/api/link", None, "").0, 501);
    assert_eq!(request(port, "GET", "/api/settings", Some(TOKEN), "").0, 404);
    assert_eq!(request(port, "POST", "/api/config-file", Some(TOKEN), r#"{"text": "{}"}"#).0, 501);
    assert_eq!(read_json(&t.path().join("receiver.json"))["label"], "before");
    engine.stop();
}

#[test]
fn events_reach_subscribers_and_stop_is_final() {
    let (c, t) = (clock(), Tmp::new("handle3"));
    let h = headend(&c);
    let engine = start(t.path(), &h, None);
    let rx = engine.subscribe();
    let ev = json!({"event_id": "h1", "state": "closed", "start_ts": c.get(), "end_ts": c.get() + 2.0,
                    "freq": 457.9375, "ch": 9, "kind": "telemetry", "peak_rssi": 300, "_source": "sdr"});
    engine.agent().unwrap().emit(ev.as_object().unwrap().clone());
    let got = rx.recv_timeout(Duration::from_secs(5)).into_iter()
        .chain(rx.try_iter()).find(|e| matches!(e, EngineEvent::Event(_)));
    match got {
        Some(EngineEvent::Event(v)) => {
            assert_eq!(v["event_id"], "h1");
            assert!(v.get("_source").is_none()); // what is queued, without the private keys
        }
        other => panic!("no event: {other:?}"),
    }
    engine.stop();
    assert!(rx.try_iter().any(|e| e == EngineEvent::Stopped));
    assert!(engine.agent().is_none());
    assert_eq!(engine.status()["stopped"], true);
    assert_eq!(read_json(&t.path().join("state/status.json"))["stopped"], true);
    engine.stop(); // twice is fine
    assert!(engine.apply_settings(json!({"label": "late"})).is_err());
}
