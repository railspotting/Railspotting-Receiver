//! State files written by earlier receiver versions, kept in tests/data.
//!
//! The store reads those databases and files, and the same operations store the same rows
//! (payload text included) byte for byte. Production state dirs were made by those versions.
mod common;
use common::tmp;
use railspotting_common::FakeClock;
use railspotting_store::{identity, jsonfmt, History, Outbox, RoleAssigner};
use rusqlite::types::Value as Sql;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

fn data() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data")
}

fn load(name: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(data().join(name)).unwrap()).unwrap()
}

/// Rows as JSON, so they compare with the expected rows in expected.json.
fn rows(path: &Path, sql: &str) -> Value {
    let db = rusqlite::Connection::open(path).unwrap();
    let mut st = db.prepare(sql).unwrap();
    let n = st.column_count();
    let out: Vec<Value> = st
        .query_map([], |r| {
            Ok(Value::Array((0..n).map(|i| match r.get::<_, Sql>(i).unwrap() {
                Sql::Null => Value::Null,
                Sql::Integer(x) => json!(x),
                Sql::Real(x) => json!(x),
                Sql::Text(t) => json!(t),
                Sql::Blob(_) => json!("<blob>"),
            }).collect()))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    Value::Array(out)
}

/// Copy a stored state file somewhere writable (opening in WAL mode may touch them).
fn state_copy(name: &str) -> PathBuf {
    let d = tmp(&format!("old-{name}"));
    std::fs::copy(data().join(name), d.join(name)).unwrap();
    d.join(name)
}

#[test]
fn same_operations_store_the_same_rows() {
    let ops = load("ops.json");
    let want = load("expected.json");
    let t0 = ops["t0"].as_f64().unwrap();
    let d = tmp("parity");

    let clock = FakeClock::new(t0);
    let ob = Outbox::with(d.join("outbox.db"), ops["cap"].as_i64().unwrap(), clock.clock()).unwrap();
    for op in ops["outbox"].as_array().unwrap() {
        clock.advance(0.5);
        match op[0].as_str().unwrap() {
            "event" => ob.add_event(&op[1]).unwrap(),
            "report" => ob.add_report(&op[1]).unwrap(),
            "ack" => {
                let batch = ob.next_events(op[1].as_i64().unwrap()).unwrap();
                let ids: Vec<&str> = op[2].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
                ob.ack_events(&batch, &ids).unwrap();
            }
            "ack_dropped" => ob.ack_dropped(op[1].as_i64().unwrap()).unwrap(),
            other => panic!("{other}"),
        }
    }
    ob.close();
    let p = d.join("outbox.db");
    assert_eq!(rows(&p, "SELECT id, kind, event_id, state, payload, created FROM items ORDER BY id"), want["outbox_items"]);
    assert_eq!(rows(&p, "SELECT k, v, typeof(v) FROM meta ORDER BY k"), want["outbox_meta"]);
    assert_eq!(rows(&p, "SELECT type, name, sql FROM sqlite_master ORDER BY name"), want["schema_outbox"]);

    let clock = FakeClock::new(t0);
    let h = History::with(d.join("history.db"), 7.0, clock.clock()).unwrap();
    for op in ops["history"].as_array().unwrap() {
        clock.advance(op.get(2).and_then(Value::as_f64).unwrap_or(1.0));
        match op[0].as_str().unwrap() {
            "event" => h.add_event(&op[1]).unwrap(),
            _ => h.add_signal(&op[1]).unwrap(),
        }
    }
    h.close();
    let p = d.join("history.db");
    assert_eq!(rows(&p, "SELECT event_id, start_ts, kind, payload FROM events ORDER BY event_id"), want["history_events"]);
    assert_eq!(rows(&p, "SELECT ts, payload FROM signal ORDER BY ts"), want["history_signal"]);
    assert_eq!(rows(&p, "SELECT type, name, sql FROM sqlite_master ORDER BY name"), want["schema_history"]);

    let mut a = RoleAssigner::new(&["vhf", "eot", "eot"], &d, FakeClock::new(t0).clock());
    a.update(&["30002222", "30001111", "11112222"]);
    assert_eq!(std::fs::read_to_string(d.join("sdr_roles.json")).unwrap(), want["sdr_roles"].as_str().unwrap());

    identity::save_identity(&d, &json!({"device_id": "rs-12ab34cd56ef", "mac": "b8:27:eb:1a:2b:3c",
                                        "interface": "eth0", "linked_at": 1790865400, "note": "caf\u{e9}"})).unwrap();
    assert_eq!(std::fs::read_to_string(d.join("identity.json")).unwrap(), want["identity"].as_str().unwrap());
}

#[test]
fn reads_earlier_state_files() {
    let ob = Outbox::open(state_copy("outbox.db")).unwrap();
    assert_eq!(ob.depth().unwrap(), 6);
    assert_eq!(ob.dropped().unwrap(), 1); // 2 dropped by the cap (stored as text), 1 acknowledged
    let evs = ob.next_events(10).unwrap();
    assert_eq!(evs.iter().map(|(_, e)| e["event_id"].as_str().unwrap()).collect::<Vec<_>>(), vec!["e1", "e3", "e4", "e5"]);
    // the stored text parses back to the same JSON the Rust side writes
    assert_eq!(jsonfmt::dumps(&evs[0].1), load("expected.json")["outbox_items"][0][4].as_str().unwrap());
    assert_eq!(ob.next_reports(20).unwrap().len(), 2);
    ob.ack_events(&evs, &["e1"]).unwrap();
    assert_eq!(ob.depth().unwrap(), 5);

    let h = History::open(state_copy("history.db")).unwrap();
    let evs = h.events(100, None, None).unwrap();
    assert_eq!(evs.iter().map(|e| e["event_id"].as_str().unwrap()).collect::<Vec<_>>(), vec!["b", "a"]);
    assert_eq!(evs[1]["state"], "closed");
    assert_eq!(h.signal(0.0).unwrap().len(), 2);

    let d = tmp("old-roles");
    std::fs::copy(data().join("sdr_roles.json"), d.join("sdr_roles.json")).unwrap();
    let a = RoleAssigner::new(&["vhf", "eot", "eot"], &d, FakeClock::new(0.0).clock());
    assert_eq!(a.assigned, vec![Some("11112222".to_string()), Some("30001111".to_string()), Some("30002222".to_string())]);
    assert_eq!(a.keys, vec!["vhf", "eot", "eot-2"]);

    std::fs::copy(data().join("identity.json"), d.join("identity.json")).unwrap();
    let ident = identity::load_or_create_identity(&d, Some("Linux"), &d.join("none")).unwrap();
    assert_eq!(ident["device_id"], "rs-12ab34cd56ef");
    assert_eq!(ident["note"], "caf\u{e9}");
}
