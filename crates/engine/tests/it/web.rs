//! The status page.



use crate::common::*;
use railspotting_engine::web::WebUI;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::sync::Arc;

/// (status, content type, body) of one request.
fn request(port: u16, method: &str, path: &str) -> (u16, String, Vec<u8>) {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
    let body = if method == "POST" { "{}" } else { "" };
    write!(s, "{method} {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
    let mut buf = vec![];
    s.read_to_end(&mut buf).unwrap();
    let split = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&buf[..split]).to_string();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    let ctype = head.lines().find(|l| l.to_ascii_lowercase().starts_with("content-type:"))
        .map(|l| l[13..].trim().to_string()).unwrap_or_default();
    let mut body = buf[split + 4..].to_vec();
    if head.to_ascii_lowercase().contains("transfer-encoding: chunked") {
        body = dechunk(&body);
    }
    (status, ctype, body)
}

fn dechunk(b: &[u8]) -> Vec<u8> {
    let (mut out, mut i) = (vec![], 0);
    while let Some(eol) = b[i..].windows(2).position(|w| w == b"\r\n") {
        let n = usize::from_str_radix(std::str::from_utf8(&b[i..i + eol]).unwrap().trim(), 16).unwrap_or(0);
        if n == 0 {
            break;
        }
        out.extend_from_slice(&b[i + eol + 2..i + eol + 2 + n]);
        i += eol + 2 + n + 2;
    }
    out
}

fn get_json(port: u16, path: &str) -> Value {
    serde_json::from_slice(&request(port, "GET", path).2).unwrap()
}

#[test]
fn routes_read_only_and_port_in_use() {
    let (c, t) = (clock(), Tmp::new("web1"));
    let h = headend(&c);
    let agent = make_agent_with(t.path(), Some(&h), Some(&c), json!({"sdr": {"devices": [vhf()]}}), true, Default::default());
    let mut ui = WebUI::new(Arc::downgrade(&agent), "127.0.0.1", 0);
    assert!(ui.start());
    let port = ui.port;
    let ev = json!({"event_id": "e1", "state": "closed", "start_ts": c.get(), "end_ts": c.get() + 2.0,
                    "freq": 457.9375, "ch": 9, "kind": "telemetry", "peak_rssi": 300});
    agent.emit(ev.as_object().unwrap().clone());
    agent.history.add_signal(&json!({"ts": c.get(), "ref_snr_db": 33.0, "sdrs": []})).unwrap();

    let (status, ctype, body) = request(port, "GET", "/");
    assert!(status == 200 && ctype.starts_with("text/html") && String::from_utf8_lossy(&body).contains("<title>RailSpotting Receiver</title>"));

    let st = get_json(port, "/api/status");
    assert!(st["device_id"] == "rs-b827eb1a2b3c" && st["sdrs"][0]["serial"] == "30002222");

    let evs = get_json(port, "/api/events?kind=telemetry&limit=5");
    assert_eq!(evs.as_array().unwrap().iter().map(|e| e["event_id"].clone()).collect::<Vec<_>>(), [json!("e1")]);
    assert_eq!(get_json(port, "/api/events?kind=voice"), json!([]));
    assert_eq!(get_json(port, "/api/signal?hours=1")[0]["ref_snr_db"], 33.0);
    assert_eq!(request(port, "GET", "/nope").0, 404);

    // the page is read only: profiles can be viewed, never changed or tested without the token
    assert_eq!(request(port, "POST", "/api/status").0, 501);
    let (status, _, _) = request(port, "GET", "/api/profiles");
    assert!(status == 200 || status == 400, "{status}");
    for path in ["/api/profiles/save", "/api/profiles/select", "/api/profiles/delete", "/api/profiles/test"] {
        assert_eq!(request(port, "POST", path).0, 501, "{path}");
    }

    // a port in use does not stop the agent
    let mut second = WebUI::new(Arc::downgrade(&agent), "127.0.0.1", port);
    assert!(!second.start());
    ui.stop();
}

#[test]
fn tabs_numbers_itc_changes_and_logs() {
    let (c, t) = (clock(), Tmp::new("web-tabs"));
    let h = headend(&c);
    let agent = make_agent_with(t.path(), Some(&h), Some(&c), json!({"sdr": {"devices": [vhf()]}}), true, Default::default());
    let route = |path: &str, q: &str| {
        let (status, _, body) = railspotting_engine::web::route(&agent, path, q);
        (status, serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null))
    };
    let now = c.get();
    for (id, dt, decoded) in [("t1", -100.0, true), ("t2", -50.0, false)] {
        let mut ev = json!({"event_id": id, "state": "closed", "start_ts": now + dt, "end_ts": now + dt + 0.2,
                            "freq": 457.9375, "ch": 9, "kind": "telemetry", "peak_rssi": 240});
        if decoded {
            ev["telemetry"] = json!({"unit_id": 12345, "brake_pipe_psi": 88, "motion": false, "corrected_bits": 0});
        }
        agent.emit(ev.as_object().unwrap().clone());
    }
    let (status, eot) = route("/api/feature", "name=eot&hours=1");
    assert_eq!(status, 200);
    assert_eq!((eot["total"].as_u64(), eot["decoded"].as_u64()), (Some(2), Some(1)));
    assert_eq!(eot["units"][0]["unit_id"], 12345);
    assert_eq!(route("/api/feature", "name=voice&hours=1").1["total"], 0);
    assert_eq!(route("/api/feature", "name=nope").0, 400);

    // wayside state changes saved by the ITC receiver, read back with no ITC dongle running
    let dir = agent.state_dir.join("itc").join("changes");
    std::fs::create_dir_all(&dir).unwrap();
    let change = |dt: f64, first: bool| json!({"t": now + dt, "wiu": "1234560ff0", "codes": [3], "first": first}).to_string();
    let day = railspotting_engine::util::utc_stamp(now, "%Y-%m-%d");
    std::fs::write(dir.join(format!("{day}.jsonl")), format!("{}\n{}\n", change(-600.0, true), change(-60.0, false))).unwrap();
    let (status, itc) = route("/api/itc", "hours=1");
    assert_eq!(status, 200);
    assert_eq!(itc["changes"], 1, "a first state is not a change");
    assert_eq!(itc["live"], Value::Null);
    assert_eq!(itc["units"][0]["wiu"], "1234560ff0");
    assert_eq!(itc["history"].as_array().unwrap().len(), 2);

    railspotting_engine::logging::setup_for_tests();
    log::warn!("ITC: a line for the logs test"); // other crates log here from WARNING up
    let (_, logs) = route("/api/logs", "topic=itc&limit=50");
    assert!(logs.as_array().unwrap().iter().any(|l| l["text"] == "ITC: a line for the logs test"), "{logs}");
    let (_, eot_logs) = route("/api/logs", "topic=eot&limit=50");
    assert!(!eot_logs.as_array().unwrap().iter().any(|l| l["text"] == "ITC: a line for the logs test"));
}

#[test]
fn link_qr_only_while_waiting() {
    let (c, t) = (clock(), Tmp::new("web2"));
    let h = headend(&c);
    let agent = unlinked(t.path(), &h, &c);
    let mut ui = WebUI::new(Arc::downgrade(&agent), "127.0.0.1", 0);
    assert!(ui.start());
    assert_eq!(request(ui.port, "GET", "/link-qr.svg").0, 404);
    agent.link_step();
    let (status, ctype, body) = request(ui.port, "GET", "/link-qr.svg");
    assert!(status == 200 && ctype == "image/svg+xml" && body.starts_with(b"<svg"));
    let st = get_json(ui.port, "/api/status");
    assert_eq!(st["registration"], "waiting");
    assert_eq!(&st["link"]["user_code"].as_str().unwrap()[4..5], "-");
    ui.stop();
}

#[test]
fn history_keeps_latest_state_and_prunes() {
    let c = clock();
    let t = Tmp::new("web3");
    let h = railspotting_store::history::History::with(t.path().join("h.db"), 1.0, c.clock()).unwrap();
    let now = c.get();
    h.add_event(&json!({"event_id": "a", "start_ts": now - 10.0, "kind": "voice", "state": "open"})).unwrap();
    h.add_event(&json!({"event_id": "a", "start_ts": now - 10.0, "kind": "voice", "state": "closed"})).unwrap();
    h.add_event(&json!({"event_id": "b", "start_ts": now - 5.0, "kind": "telemetry", "state": "closed"})).unwrap();
    let ids = |v: Vec<Value>| v.iter().map(|e| e["event_id"].clone()).collect::<Vec<_>>();
    assert_eq!(ids(h.events(100, None, None).unwrap()), [json!("b"), json!("a")]);
    assert_eq!(h.events(100, None, Some("voice")).unwrap()[0]["state"], "closed");
    assert_eq!(ids(h.events(100, Some(now - 6.0), None).unwrap()), [json!("a")]);
}

#[test]
fn recordings_are_listed_and_only_recordings_are_served() {
    use railspotting_engine::web::{recording_path, recordings};
    let t = crate::common::Tmp::new("rec");
    let st = t.path();
    let wav = |dir: &str, name: &str, meta: serde_json::Value| {
        std::fs::create_dir_all(st.join(dir)).unwrap();
        std::fs::write(st.join(dir).join(format!("{name}_voice.wav")), vec![0u8; 44 + 32_000 * 2]).unwrap();
        std::fs::write(st.join(dir).join(format!("{name}_voice.json")), meta.to_string()).unwrap();
    };
    wav("voice_clips", "20261012-195805.363", json!({"transcript": null}));
    wav("detector_candidates/csx-68.1", "20261012-195805.363",
        json!({"detector_id": "csx-68.1", "reference_text": "CSX equipment defect detector", "event": {"freq": 161.1}}));
    wav("detector_clips", "20261012-214927.598", json!({"transcript": {"text": "Norfolk Southern, milepost 475.8",
        "_detector": {"detector_id": "ns-475.8"}}}));
    wav("watch_clips/161.1000", "20261012-190312.524", json!({"freq": 161.1}));
    std::fs::write(st.join("identity.json"), "{}").unwrap();
    std::fs::write(st.join("voice_clips").join("notes.txt"), "x").unwrap();

    let all = recordings(st, None, 100);
    // the copy in voice_clips is listed once, under its detector candidate
    assert_eq!(all.len(), 3, "{all:?}");
    let cand = all.iter().find(|r| r["source"] == "candidate").unwrap();
    assert!(cand["detector"] == "csx-68.1" && cand["freq"] == 161.1 && cand["seconds"] == 2.0
            && cand["url"] == "/recordings/detector_candidates/csx-68.1/20261012-195805.363_voice.wav", "{cand}");
    assert_eq!(cand["text"], "CSX equipment defect detector");
    let det = all.iter().find(|r| r["source"] == "detector").unwrap();
    assert!(det["detector"] == "ns-475.8" && det["text"] == "Norfolk Southern, milepost 475.8");
    let watched = all.iter().find(|r| r["source"] == "watched").unwrap();
    assert_eq!(watched["freq"], 161.1);
    assert_eq!(recordings(st, Some("detector"), 100).len(), 1);
    assert_eq!(recordings(st, Some("detector,candidate"), 100).len(), 2);
    assert_eq!(recordings(st, None, 2).len(), 2);

    assert!(recording_path(st, "detector_candidates/csx-68.1/20261012-195805.363_voice.wav").is_some());
    assert!(recording_path(st, "voice_clips/20261012-195805.363_voice.wav").is_some());
    for bad in ["identity.json", "voice_clips/notes.txt", "voice_clips/../identity.json", "../etc/passwd",
                "detector_candidates/../voice_clips/20261012-195805.363_voice.wav",
                "detector_candidates/20261012-195805.363_voice.wav", "voice_clips/x/20261012-195805.363_voice.wav",
                "voice_clips/20261012-195805.363_voice.json", "state/voice_clips/20261012-195805.363_voice.wav",
                "voice_clips/20261012-999999.999_voice.wav"] {
        assert!(recording_path(st, bad).is_none(), "{bad}");
    }
}
