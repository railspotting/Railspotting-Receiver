//! Fixtures in the repo: fixture JSON written back byte for byte, and v1.5 detector objects
//! round tripping through the API. Also the credentials.json rules.

mod common;

use common::*;
use railspotting_proto::{dumps, keys, jsonfmt};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

fn fixture_jsons(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let p = entry.path();
        if p.is_dir() {
            if p.file_name().is_some_and(|n| n != "local") {
                fixture_jsons(&p, out);
            }
        } else if p.extension().is_some_and(|e| e == "json") {
            out.push(p);
        }
    }
}

/// The fixtures are written with an indent of 1 and a final newline: reading and writing every
/// fixture gives the same bytes, so key order, floats and escapes all match.
#[test]
fn fixture_json_round_trips_byte_for_byte() {
    let mut files = vec![];
    fixture_jsons(&fixtures(), &mut files);
    assert!(files.iter().any(|p| p.ends_with("api/vectors.json")));
    for p in files {
        let text = std::fs::read_to_string(&p).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        // the detector template is a copy of the production one, written with indent=2
        let same = [1, 2].iter().any(|n| {
            let out = jsonfmt::dumps_indent(&v, *n);
            text == out || text == out + "\n"
        });
        if !same {
            let out = jsonfmt::dumps_indent(&v, 2);
            let i = out.bytes().zip(text.bytes()).position(|(a, b)| a != b).unwrap_or(0);
            panic!("{} differs at {i}: {:?} vs {:?}", p.display(), &out[i.saturating_sub(60)..(i + 40).min(out.len())],
                   &text[i.saturating_sub(60)..(i + 40).min(text.len())]);
        }
    }
}

#[test]
fn v1_5_detector_objects_round_trip() {
    let all = vectors();
    let objs = all["v1_5"]["valid_detector_objects"].as_array().unwrap().clone();
    assert_eq!(objs.len(), 5);
    let keys_order = ["detector_id", "railroad", "milepost", "result", "method", "scores", "length_s",
                      "all_clear_length_s", "repeat_of", "heard_text"];
    for o in &objs {
        assert_eq!(o.as_object().unwrap().keys().map(String::as_str).collect::<Vec<_>>(), keys_order);
        let text = dumps(o);
        assert_eq!(&serde_json::from_slice::<Value>(&text).unwrap(), o);
    }
    assert_eq!(String::from_utf8(dumps(&objs[4])).unwrap(),
               r#"{"detector_id":"ns-475.8","railroad":"NS","milepost":475.8,"result":"incomplete","method":"opening","scores":{"opening":0.8,"ending":null,"rhythm":null},"length_s":6.0,"all_clear_length_s":9.2,"repeat_of":null,"heard_text":null}"#);
    // through the client and the fake headend, inside an event
    let c = clock();
    let h = headend(&c);
    let api = approved_client(&h, &c);
    let events: Vec<Value> = objs.iter().enumerate().map(|(i, d)| json!({
        "event_id": format!("e{i}"), "state": "closed", "start_ts": 1.0, "end_ts": 10.2, "freq": 160.905,
        "ch": 1, "kind": "voice", "detector": d})).collect();
    api.post_events(&events).unwrap();
    let st = h.state();
    for (i, d) in objs.iter().enumerate() {
        assert_eq!(&st.events[&format!("e{i}")]["detector"], d);
    }
    assert_eq!(st.raw_bodies.last().unwrap(), &dumps(&json!({"events": events})));
}

#[test]
fn credentials_rules() {
    let dir = tmpdir("rules");
    let creds = dir.join("credentials.json");
    assert_eq!(keys::load_credentials(&creds).unwrap(), Map::new()); // missing is empty
    let k1 = keys::load_or_create_key(&creds, false).unwrap();
    let k2 = keys::load_or_create_key(&creds, false).unwrap();
    assert_eq!(k1.to_bytes(), k2.to_bytes());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&creds, std::fs::Permissions::from_mode(0o644)).unwrap();
        keys::load_or_create_key(&creds, false).unwrap();
        assert_eq!(std::fs::metadata(&creds).unwrap().permissions().mode() & 0o777, 0o600);
    }
    let mut m = keys::load_credentials(&creds).unwrap();
    m.insert("server".into(), json!("x"));
    keys::save_credentials(&creds, &m).unwrap();
    let k3 = keys::replace_key(&creds).unwrap();
    assert_ne!(k3.to_bytes(), k1.to_bytes());
    let m = keys::load_credentials(&creds).unwrap();
    assert_eq!(m["server"], "x");
    assert_eq!(m["private_key"], keys::private_to_b64(&k3));
    assert!(!dir.join("credentials.tmp").exists());
    std::fs::write(&creds, "not json").unwrap();
    assert!(keys::load_or_create_key(&creds, false).is_err());
    // b64url with or without padding
    assert_eq!(keys::b64url_decode("AAEC").unwrap(), [0, 1, 2]);
    assert_eq!(keys::b64url_decode("AAE").unwrap(), [0, 1]);
    assert_eq!(keys::b64url_decode("AAE=").unwrap(), [0, 1]);
    assert_eq!(keys::b64url_decode("-_8").unwrap(), [0xfb, 0xff]);
    assert!(keys::private_from_seed(&[0; 31]).is_err());
    let _ = std::fs::remove_dir_all(dir);
}
