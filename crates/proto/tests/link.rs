//! API contract v1.4: device linking (test vectors, codes and the client against the headend).
//! The agent's linking state machine is tested in the engine crate.

mod common;

use common::*;
use railspotting_proto::fake::FakeHeadend;
use railspotting_proto::{dumps, keys, link, ApiClient};
use serde_json::{json, Value};

const START_BODY: &[u8] = br#"{"public_key":"A6EHv_POEL4dcN0Y50vAmWfk1jCbpQ1fHdyGZBJVMbg","mac":"b8:27:eb:12:34:56","hostname":"rx-garage","platform":"Linux-6.6.31-rpi-aarch64","agent_version":"1.4.0"}"#;

// test vectors (contract section 4) -------------------------------------------------------

#[test]
fn link_start_vector() {
    let all = vectors();
    let v = &all["v1_4"];
    let key = keys::private_from_seed(&hex::decode(v["seed_hex"].as_str().unwrap()).unwrap()).unwrap();
    assert_eq!(keys::public_b64(&key), "A6EHv_POEL4dcN0Y50vAmWfk1jCbpQ1fHdyGZBJVMbg");
    let body = json!({"public_key": "A6EHv_POEL4dcN0Y50vAmWfk1jCbpQ1fHdyGZBJVMbg", "mac": "b8:27:eb:12:34:56",
                      "hostname": "rx-garage", "platform": "Linux-6.6.31-rpi-aarch64",
                      "agent_version": "1.4.0"});
    assert_eq!(dumps(&body), START_BODY);
    let ls = &v["link_start"];
    assert_eq!(ls["body"].as_str().unwrap().as_bytes(), START_BODY);
    assert_eq!(keys::sha256_hex(START_BODY), "6a01bbfbb4545bf79ed28a54aba56ab381e83491f9521e95879449177b928a12");
    assert_eq!(keys::canonical_string("POST", "/api/v1/link", 1791034500, "00112233445566778899aabbccddeeff",
                                      START_BODY),
               "POST\n/api/v1/link\n1791034500\n00112233445566778899aabbccddeeff\n\
                6a01bbfbb4545bf79ed28a54aba56ab381e83491f9521e95879449177b928a12");
    let headers = keys::signed_headers(&key, None, ls["method"].as_str().unwrap(), ls["path"].as_str().unwrap(),
                                       START_BODY, ls["timestamp"].as_i64().unwrap(), ls["nonce"].as_str());
    assert!(headers.iter().all(|(k, _)| k != "X-RS-Device"));
    let sig = &headers.iter().find(|(k, _)| k == "X-RS-Signature").unwrap().1;
    assert_eq!(sig, "iyc7IjgPhOL17huQ_51RUyvI1gqGHUJ2jgWQMfQkfFeL2DRM6APmfTK4Z16YI9spQtMzXY5B2xND1OPmG3ooAw");
    assert_eq!(sig, ls["signature"].as_str().unwrap());
}

#[test]
fn link_poll_vector() {
    let all = vectors();
    let p = &all["v1_4"]["link_poll"];
    let sig = keys::sign(&test_key(), p["method"].as_str().unwrap(), p["path"].as_str().unwrap(),
                         p["timestamp"].as_i64().unwrap(), p["nonce"].as_str().unwrap(),
                         p["body"].as_str().unwrap().as_bytes());
    assert_eq!(sig, "Ck44oYXMqCm0s42gRQqceRsJGOlaQNPF_wW4ICU268y8O5S5GO0x8Hm8h1mN2XP0T3aQH48xuKIWTLhSwgViAw");
    assert_eq!(sig, p["signature"].as_str().unwrap());
}

#[test]
fn user_codes_from_fixture() {
    let all = vectors();
    let codes = &all["v1_4"]["user_codes"];
    for (given, want) in codes["valid"].as_object().unwrap() {
        assert_eq!(link::normalize_user_code(given).as_deref(), want.as_str(), "{given}");
        assert_eq!(link::format_user_code(given), "WDJB-MJHT");
    }
    for bad in codes["invalid"].as_array().unwrap() {
        assert_eq!(link::normalize_user_code_value(bad), None, "{bad}");
    }
    assert_eq!(link::normalize_user_code_value(&Value::Null), None);
}

// client --------------------------------------------------------------------------------

fn start_body(key: &keys::PrivateKey) -> Value {
    json!({"public_key": keys::public_b64(key), "hostname": "h", "platform": "p", "agent_version": "1.1.0"})
}

#[test]
fn link_calls_are_signed_without_device_header() {
    let c = clock();
    let h = headend(&c);
    let key = test_key();
    let api = ApiClient::new(&h.url(), None, key.clone(), None, c.clock(), 5.0).unwrap();
    let (status, body) = api.link_start(&start_body(&key)).unwrap();
    assert_eq!(status, 201);
    assert_eq!(link::normalize_user_code(body["user_code"].as_str().unwrap()).unwrap().len(), 8);
    assert_eq!(body["expires_in"], 900);
    assert_eq!(body["interval"], 5);
    let complete = body["verification_uri_complete"].as_str().unwrap();
    assert!(complete.starts_with(&format!("{}?code=", body["verification_uri"].as_str().unwrap())));
    c.advance(5.0);
    assert_eq!(api.link_poll(body["link_id"].as_str().unwrap()).unwrap()["status"], "pending");
    let st = h.state();
    assert_eq!(st.link_headers.len(), 2);
    assert!(st.link_headers.iter().all(|hd| !hd.contains_key("x-rs-device")));
    assert!(st.link_headers.iter().all(|hd| hd.get("user-agent").map(String::as_str) == Some("railspotting-receiver")));
}

#[test]
fn link_calls_never_carry_a_device_id_even_if_one_is_set() {
    let c = clock();
    let h = headend(&c);
    let key = test_key();
    let api = ApiClient::new(&h.url(), Some(DEVICE_ID.into()), key.clone(), None, c.clock(), 5.0).unwrap();
    assert_eq!(api.link_start(&start_body(&key)).unwrap().0, 201);
}

fn started(c: &railspotting_common::FakeClock) -> (FakeHeadend, ApiClient, Value) {
    let h = headend(c);
    let key = keys::generate_private();
    let api = ApiClient::new(&h.url(), None, key.clone(), None, c.clock(), 5.0).unwrap();
    let (status, body) = api.link_start(&start_body(&key)).unwrap();
    assert_eq!(status, 201);
    (h, api, body)
}

#[test]
fn approved_link_gives_a_locally_administered_id() {
    let c = clock();
    let (h, api, body) = started(&c);
    let code = body["user_code"].as_str().unwrap().to_lowercase(); // the owner types it any way
    let assigned = h.approve_link(&code, None).unwrap();
    let first = u8::from_str_radix(&assigned[3..5], 16).unwrap();
    assert!(first & 0x02 != 0 && first & 0x01 == 0);
    assert_eq!(assigned.len(), 15);
    c.advance(5.0);
    let poll = api.link_poll(body["link_id"].as_str().unwrap()).unwrap();
    assert_eq!(poll, json!({"status": "approved", "device_id": assigned}));
    // runs as v1 with X-RS-Device from now on
    api.set_device_id(Some(assigned));
    api.heartbeat(&json!({"ts": c.get()})).unwrap();
    assert_eq!(h.state().heartbeats.len(), 1);
    // the same key starting again is told its ID at once
    let (status, again) = api.link_start(&start_body(api.key())).unwrap();
    assert_eq!(status, 200);
    assert_eq!(again["status"], "approved");
}

#[test]
fn restart_while_waiting_gets_the_same_code() {
    let c = clock();
    let (h, api, body) = started(&c);
    c.advance(120.0);
    let (status, again) = api.link_start(&start_body(api.key())).unwrap();
    assert_eq!(status, 201);
    assert_eq!(again["user_code"], body["user_code"]);
    assert_eq!(again["expires_in"], 780);
    assert_eq!(h.state().links.len(), 1);
}

#[test]
fn slow_down_and_expiry() {
    let c = clock();
    let (h, api, body) = started(&c);
    let lid = body["link_id"].as_str().unwrap();
    c.advance(5.0);
    api.link_poll(lid).unwrap();
    c.advance(2.0); // too soon
    let e = api.link_poll(lid).unwrap_err();
    assert_eq!((e.status, e.error.as_str()), (Some(429), "slow_down"));
    c.advance(10.0);
    assert_eq!(api.link_poll(lid).unwrap()["expires_in"], 900 - 17);
    h.state().links.get_mut(lid).unwrap().expires = c.get() + 10.0;
    c.advance(10.0);
    assert_eq!(api.link_poll(lid).unwrap(), json!({"status": "expired"}));
}

#[test]
fn denied_and_not_found() {
    let c = clock();
    let (h, api, body) = started(&c);
    let lid = body["link_id"].as_str().unwrap();
    h.deny_link(body["user_code"].as_str().unwrap());
    c.advance(5.0);
    assert_eq!(api.link_poll(lid).unwrap(), json!({"status": "denied"}));
    h.state().links.clear();
    c.advance(5.0);
    let e = api.link_poll(lid).unwrap_err();
    assert_eq!((e.status, e.error.as_str()), (Some(404), "not_found"));
}

#[test]
fn poll_signed_by_another_key_is_not_found() {
    let c = clock();
    let (h, _api, body) = started(&c);
    let other = ApiClient::new(&h.url(), None, keys::generate_private(), None, c.clock(), 5.0).unwrap();
    let e = other.link_poll(body["link_id"].as_str().unwrap()).unwrap_err();
    assert_eq!((e.status, e.error.as_str()), (Some(404), "not_found"));
}

#[test]
fn bad_public_key_is_a_bad_request() {
    let c = clock();
    let h = headend(&c);
    let api = ApiClient::new(&h.url(), None, test_key(), None, c.clock(), 5.0).unwrap();
    let e = api.link_start(&json!({"public_key": "abc"})).unwrap_err();
    assert_eq!((e.status, e.error.as_str(), e.message.as_str()), (Some(400), "bad_request", "public_key"));
    // signed by a key other than the one in the body
    let e = api.link_start(&start_body(&keys::generate_private())).unwrap_err();
    assert_eq!((e.status, e.error.as_str()), (Some(401), "bad_signature"));
}

#[test]
fn network_failure_while_linking() {
    let c = clock();
    let (h, api, body) = started(&c);
    h.set_down(true);
    c.advance(5.0);
    assert!(api.link_poll(body["link_id"].as_str().unwrap()).unwrap_err().network());
    h.set_down(false);
    c.advance(5.0);
    assert_eq!(api.link_poll(body["link_id"].as_str().unwrap()).unwrap()["status"], "pending");
}

#[test]
fn invalid_assigned_id_is_passed_through() {
    // the client hands the ID over as sent; refusing it is the agent's job
    let c = clock();
    let (h, api, body) = started(&c);
    h.approve_link(body["user_code"].as_str().unwrap(), Some("rs-NOTVALID"));
    c.advance(5.0);
    assert_eq!(api.link_poll(body["link_id"].as_str().unwrap()).unwrap()["device_id"], "rs-NOTVALID");
}

#[test]
fn revoked_device_gets_403_and_links_again_with_the_same_key() {
    let c = clock();
    let h = headend(&c);
    let api = approved_client(&h, &c);
    h.state().devices.get_mut(DEVICE_ID).unwrap().status = "revoked".into();
    let e = api.heartbeat(&json!({"ts": c.get()})).unwrap_err();
    assert!(e.not_allowed() && e.error == "revoked");
    api.set_device_id(None);
    let (status, _) = api.link_start(&start_body(api.key())).unwrap();
    assert_eq!(status, 201);
    assert_eq!(h.state().link_starts.last().unwrap()["public_key"], keys::public_b64(&test_key()));
}

#[test]
fn never_approved_v1_receiver_gets_403_or_401() {
    for (status, error) in [(403, "pending"), (403, "rejected"), (401, "unknown_device")] {
        let c = clock();
        let h = headend(&c);
        let api = approved_client(&h, &c);
        if error == "unknown_device" {
            h.state().devices.remove(DEVICE_ID);
        } else {
            h.state().devices.get_mut(DEVICE_ID).unwrap().status = error.into();
        }
        let e = api.heartbeat(&json!({})).unwrap_err();
        assert_eq!((e.status, e.error.as_str()), (Some(status), error));
    }
}
