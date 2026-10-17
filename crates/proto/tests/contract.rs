//! API contract v1: test vectors, signing, error handling and the privacy rule.
//! The client and headend parts; agent tests live in the engine crate.
//!
//! v1.4 linking has its own tests in link.rs.

mod common;

use common::*;
use railspotting_proto::keys;
use railspotting_proto::{dumps, ApiClient, Backoff};
use serde_json::json;

#[test]
fn public_key() {
    let v = vectors();
    assert_eq!(keys::public_b64(&test_key()), v["v1"]["public_key_b64url"]);
    assert_eq!(v["v1"]["private_key_seed_hex"], SEED_HEX);
}

#[test]
fn canonical_string_first_vector() {
    let all = vectors();
    let v = &all["v1"]["vectors"][0];
    assert_eq!(
        keys::canonical_string(v["method"].as_str().unwrap(), v["path"].as_str().unwrap(),
                               v["timestamp"].as_i64().unwrap(), v["nonce"].as_str().unwrap(), b""),
        "GET\n/api/v1/config\n1790865400\n00112233445566778899aabbccddeeff\n\
         e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(keys::sha256_hex(b""), keys::EMPTY_SHA256);
}

#[test]
fn signature_vectors() {
    let all = vectors();
    let key = test_key();
    let vs = all["v1"]["vectors"].as_array().unwrap();
    assert_eq!(vs.len(), 2);
    for v in vs {
        let body = v["body"].as_str().unwrap().as_bytes();
        let (m, p, ts, n) = (v["method"].as_str().unwrap(), v["path"].as_str().unwrap(),
                             v["timestamp"].as_i64().unwrap(), v["nonce"].as_str().unwrap());
        assert_eq!(keys::sha256_hex(body), v["body_sha256"]);
        assert!(keys::canonical_string(m, p, ts, n, body).ends_with(v["body_sha256"].as_str().unwrap()));
        assert_eq!(keys::sign(&key, m, p, ts, n, body), v["signature"]);
        // and the headend's check accepts it
        assert!(keys::verify(&keys::public_bytes(&key), &keys::canonical_string(m, p, ts, n, body),
                             v["signature"].as_str().unwrap()));
    }
    // the heartbeat vector's body is what dumps writes for that object
    assert_eq!(dumps(&json!({"ts": 1790865400.0, "uptime_s": 60})), vs[1]["body"].as_str().unwrap().as_bytes());
}

#[test]
fn signed_headers_v1() {
    let key = test_key();
    let h = keys::signed_headers(&key, Some(DEVICE_ID), "get", "/api/v1/config", b"", 1790865400,
                                 Some("00112233445566778899aabbccddeeff"));
    let names: Vec<&str> = h.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(names, ["X-RS-Device", "X-RS-Timestamp", "X-RS-Nonce", "X-RS-Signature"]);
    assert_eq!(h[3].1, vectors()["v1"]["vectors"][0]["signature"]);
    let fresh = keys::signed_headers(&key, Some(""), "GET", "/x", b"", 1, None);
    assert_eq!(fresh.len(), 3);
    assert_eq!(fresh[1].1.len(), 32); // token_hex(16)
}

#[test]
fn signed_request_matches_headend() {
    let c = clock();
    let h = headend(&c);
    let api = approved_client(&h, &c);
    let (cfg, etag) = api.get_config(None).unwrap();
    assert_eq!(cfg.unwrap()["config_version"], "7");
    assert_eq!(etag.as_deref(), Some("\"7\""));
    assert_eq!(api.get_config(etag.as_deref()).unwrap(), (None, etag.clone())); // 304 with If-None-Match
    let resp = api.heartbeat(&json!({"ts": c.get(), "uptime_s": 1})).unwrap();
    assert_eq!(resp["config_version"], "7");
    assert!(api.offset().unwrap().abs() <= 1.0);
    assert_eq!(api.clock_synced(), Some(true));
    let lr = api.last_results();
    assert_eq!(lr["/heartbeat"], json!({"ts": 1790865400.0, "status": 200, "error": null}));
    assert_eq!(lr["/config"]["status"], 304);
}

#[test]
fn body_bytes_are_what_was_signed() {
    let c = clock();
    let h = headend(&c);
    let api = approved_client(&h, &c);
    let report = json!({"type": "radio_check", "ts": 1.0, "data": {"ok": true}});
    api.post_report(&report).unwrap();
    let st = h.state();
    assert_eq!(st.raw_bodies.last().unwrap(), &dumps(&report));
    assert_eq!(st.raw_bodies.last().unwrap(), br#"{"type":"radio_check","ts":1.0,"data":{"ok":true}}"#);
    assert_eq!(st.reports.last().unwrap()["type"], "radio_check");
}

#[test]
fn error_responses_raise() {
    for (status, error) in [(401, "bad_signature"), (401, "unknown_device"), (401, "replay"), (403, "pending"),
                            (403, "revoked"), (429, "rate_limited")] {
        let c = clock();
        let h = headend(&c);
        let api = approved_client(&h, &c);
        let headers: &[(&str, &str)] = if status == 429 { &[("Retry-After", "42")] } else { &[] };
        h.fail_next(status, error, headers);
        let e = api.get_commands().unwrap_err();
        assert_eq!((e.status, e.error.as_str()), (Some(status), error));
        assert_eq!(e.message, "injected");
        assert_eq!(e.auth_failed(), status == 401);
        assert_eq!(e.not_allowed(), status == 403);
        assert_eq!(e.body["error"], error);
        if status == 429 {
            assert_eq!(e.retry_after, Some(42.0));
            assert!(Backoff::default().next_delay(e.retry_after) >= 42.0);
        }
        assert_eq!(api.last_results()["/commands"]["status"], status);
    }
}

#[test]
fn empty_error_message_uses_the_reason() {
    let c = clock();
    let h = headend(&c);
    let api = approved_client(&h, &c);
    // an unsigned request: the headend answers 401 unknown_device
    let e = api.request("GET", "/commands", None, false, &[], "", true).unwrap_err();
    assert_eq!((e.status, e.error.as_str()), (Some(401), "unknown_device"));
    assert_eq!(e.message, "Unauthorized"); // empty message: the status reason phrase
    assert_eq!(e.to_string(), "401 unknown_device: Unauthorized");
    // a lower case method is sent upper case, as it was signed
    assert!(api.request("get", "/commands", None, true, &[], "", true).is_ok());
    // a caller's Content-Type is replaced, not sent twice
    let hdrs = [("content-type".to_string(), "text/plain".to_string())];
    assert!(api.request("POST", "/reports", Some(&json!({"t": 1})), true, &hdrs, "", true).is_ok());
    assert_eq!(h.state().reports.last().unwrap()["t"], 1);
    // 404 for an unknown endpoint
    let e = api.request("GET", "/nope", None, true, &[], "a=1", true).unwrap_err();
    assert_eq!((e.status, e.error.as_str()), (Some(404), "not_found"));
    assert!(api.last_results().contains_key("/nope"));
}

#[test]
fn clock_skew_stops_signing() {
    let c = clock();
    let h = headend(&c);
    let key = test_key();
    h.add_device(DEVICE_ID, &keys::public_bytes(&key), "approved");
    let inner = c.clock();
    let skewed: railspotting_common::Clock = std::sync::Arc::new(move || inner() - 1000.0);
    let api = ApiClient::new(&h.url(), Some(DEVICE_ID.into()), key, None, skewed, 5.0).unwrap();
    let e = api.get_commands().unwrap_err();
    assert_eq!(e.error, "clock_skew");
    assert!(api.clock_known_bad());
    assert_eq!(api.clock_synced(), Some(false));
    let n = h.state().raw_bodies.len();
    let e = api.get_commands().unwrap_err(); // refused locally, nothing signed or sent
    assert!(e.is_clock_error() && e.network());
    assert_eq!(e.clock_offset, Some(-1000.0));
    assert_eq!(e.to_string(), "local clock is -1000 s from the server; not signing");
    assert_eq!(h.state().raw_bodies.len(), n);
}

#[test]
fn probe_clock_learns_the_offset_unsigned() {
    let c = clock();
    let h = headend(&c);
    let inner = c.clock();
    let ahead: railspotting_common::Clock = std::sync::Arc::new(move || inner() + 500.0);
    let api = ApiClient::new(&h.url(), None, test_key(), None, ahead, 5.0).unwrap();
    assert_eq!(api.offset(), None);
    assert_eq!(api.clock_synced(), None);
    api.probe_clock().unwrap();
    assert_eq!(api.offset(), Some(-500.0));
    assert!(api.clock_known_bad());
}

#[test]
fn network_failure_is_an_api_error_without_status() {
    let c = clock();
    let h = headend(&c);
    let api = approved_client(&h, &c);
    h.set_down(true);
    let e = api.heartbeat(&json!({"ts": 1})).unwrap_err();
    assert!(e.network() && e.status.is_none() && e.error == "network");
    let rec = &api.last_results()["/heartbeat"];
    assert_eq!(rec["status"], serde_json::Value::Null);
    assert!(rec["error"].is_string());
    assert!(e.message.starts_with(rec["error"].as_str().unwrap()));
    assert!(h.state().raw_bodies.is_empty());
    // nothing listening at all
    let port = h.port();
    drop(h);
    let api = ApiClient::new(&format!("http://127.0.0.1:{port}"), None, test_key(), None, c.clock(), 2.0).unwrap();
    assert!(api.probe_clock().unwrap_err().network());
}

#[test]
fn private_key_never_sent() {
    let c = clock();
    let h = headend(&c);
    let dir = tmpdir("never-sent");
    let creds = dir.join("credentials.json");
    let key = keys::load_or_create_key(&creds, true).unwrap();
    let secret = keys::private_to_b64(&key);
    let api = ApiClient::new(&h.url(), None, key.clone(), None, c.clock(), 5.0).unwrap();
    let (status, body) = api.link_start(&json!({"public_key": keys::public_b64(&key), "hostname": "h",
                                                "platform": "p", "agent_version": "2.0.0"})).unwrap();
    assert_eq!(status, 201);
    let id = h.approve_link(body["user_code"].as_str().unwrap(), None).unwrap();
    c.advance(5.0);
    let poll = api.link_poll(body["link_id"].as_str().unwrap()).unwrap();
    assert_eq!(poll["device_id"], id.as_str());
    api.set_device_id(Some(id));
    api.heartbeat(&json!({"ts": c.get()})).unwrap();
    assert!(h.state().raw_bodies.iter().all(|b| !String::from_utf8_lossy(b).contains(&secret)));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&creds).unwrap().permissions().mode() & 0o777, 0o600);
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn backoff_range() {
    let mut b = Backoff::default();
    let delays: Vec<f64> = (0..20).map(|_| b.next_delay(None)).collect();
    assert!(1.0 <= delays[0] && delays[0] <= 2.0);
    assert!(delays.iter().cloned().fold(0.0, f64::max) <= 300.0);
    b.reset();
    assert!(b.next_delay(None) <= 2.0);
}
