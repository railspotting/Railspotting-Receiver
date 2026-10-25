//! Wayside state changes and base stations, as records for the headend (contract v1.18,
//! `docs/contract-v1.18.md` in the receiver).
//!
//! Every wayside unit repeats its whole status about every 4 seconds, so of about 35,000 messages
//! an hour only a few dozen say something new. `Tracker` keeps the last state sent for each unit
//! and gives a record only when it changes, or a starting state the first time a unit is heard
//! (after a start, or after a gap). Base stations give a record when first heard and when their
//! position or channels change. No I/O.

use serde_json::{json, Value};
use std::collections::HashMap;
use uuid::Uuid;

/// A unit not heard for this long starts again with a starting state.
pub const GAP_S: f64 = 60.0;
/// A relayed reply (0x34) can arrive a cycle late, so it counts only when the unit's own status
/// message (0x33) has not been heard for this long.
pub const STATUS_RECENT_S: f64 = 20.0;

struct Unit {
    sent: Vec<i64>,
    heard: f64,
    status: f64,
}

#[derive(PartialEq)]
struct BaseState {
    lat: Option<i64>,
    lon: Option<i64>,
    local: i64,
    near: Vec<i64>,
}

pub struct Tracker {
    ns: Uuid,
    units: HashMap<String, Unit>,
    bases: HashMap<String, BaseState>,
}

fn ms(t: f64) -> i64 {
    (t * 1000.0).round() as i64
}

fn channel(v: &Value) -> Option<i64> {
    v.as_i64().filter(|c| (1..=999).contains(c))
}

/// The signal level of the message: its peak relative to full scale (`floor_db + level_db`),
/// within the contract's -150 to 20 dB.
fn level(m: &Value) -> f64 {
    let peak = m["level_db"].as_f64().unwrap_or(0.0) + m["floor_db"].as_f64().unwrap_or(0.0);
    (peak.clamp(-150.0, 20.0) * 10.0).round() / 10.0
}

impl Tracker {
    /// Record IDs are UUID v5 under `ns`, so a retry of the same record has the same ID.
    pub fn new(ns: Uuid) -> Tracker {
        Tracker { ns, units: HashMap::new(), bases: HashMap::new() }
    }

    /// One decoded message (`messages.md`): the record it makes, if any.
    pub fn add(&mut self, m: &Value) -> Option<Value> {
        if m["crc_ok"] == false {
            return None;
        }
        match m["type"].as_str()? {
            "WIU" => self.wayside(m, "status"),
            "WIU_REPLY" => self.wayside(m, "reply"),
            "BEACON" => self.base(m),
            _ => None,
        }
    }

    fn wayside(&mut self, m: &Value, source: &str) -> Option<Value> {
        let wiu = m["wiu"].as_str()?.to_ascii_lowercase();
        if wiu.len() != 10 || !wiu.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let codes: Vec<i64> = m["codes"].as_array()?.iter().map(|c| c.as_i64().filter(|c| (0..=31).contains(c))).collect::<Option<_>>()?;
        if codes.is_empty() || codes.len() > 64 {
            return None;
        }
        let t = m["t"].as_f64()?;
        let ch = channel(&m["ch"])?;
        let (first, previous) = match self.units.get_mut(&wiu) {
            Some(u) if t - u.heard < GAP_S => {
                u.heard = u.heard.max(t);
                if source == "status" {
                    u.status = u.status.max(t);
                } else if t - u.status < STATUS_RECENT_S {
                    return None; // the unit's own messages are coming in: they say its state
                }
                if u.sent == codes {
                    return None;
                }
                (false, Some(std::mem::replace(&mut u.sent, codes.clone())))
            }
            _ => {
                let status = if source == "status" { t } else { f64::NEG_INFINITY };
                self.units.insert(wiu.clone(), Unit { sent: codes.clone(), heard: t, status });
                (true, None)
            }
        };
        let joined = codes.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
        let id = Uuid::new_v5(&self.ns, format!("itc-wayside:{wiu}:{}:{joined}", ms(t)).as_bytes());
        let mut r = json!({
            "id": id.to_string(), "type": "wayside", "wiu": wiu, "t": ms(t) as f64 / 1000.0, "first": first,
            "codes": codes, "previous_codes": previous, "source": source, "channel": ch, "level_db": level(m),
        });
        if let Some(ind) = m["indicators"].as_str().filter(|s| !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_hexdigit())) {
            r["indicators"] = json!(ind.to_ascii_lowercase());
        }
        Some(r)
    }

    fn base(&mut self, m: &Value) -> Option<Value> {
        let base = m["radio"].as_str()?.to_ascii_lowercase();
        if base.is_empty() || base.len() > 16 || !base.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let t = m["t"].as_f64()?;
        let local = channel(&m["channel"])?;
        let near: Vec<i64> = m["channels"].as_array().into_iter().flatten().filter_map(channel).take(32).collect();
        let deg = |v: &Value| v.as_f64().filter(|d| d.is_finite() && d.abs() <= 180.0);
        let (lat, lon) = (deg(&m["lat"]), deg(&m["lon"]));
        // compared to 1e-5 degrees (about a metre), the precision the beacon gives
        let state = BaseState { lat: lat.map(|d| (d * 1e5).round() as i64), lon: lon.map(|d| (d * 1e5).round() as i64), local, near: near.clone() };
        if self.bases.get(&base) == Some(&state) {
            return None;
        }
        self.bases.insert(base.clone(), state);
        let id = Uuid::new_v5(&self.ns, format!("itc-base:{base}:{}", ms(t)).as_bytes());
        Some(json!({
            "id": id.to_string(), "type": "base", "base": base, "t": ms(t) as f64 / 1000.0, "lat": lat, "lon": lon,
            "local_channel": local, "neighbour_channels": near, "channel": channel(&m["ch"]), "level_db": level(m),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NS: Uuid = uuid::uuid!("6f1c4e1e-6a0b-4a52-9d55-4d3c1f0a7b15");

    fn wiu(kind: &str, unit: &str, t: f64, codes: &[i64]) -> Value {
        json!({"type": kind, "wiu": unit, "t": t, "ch": 114, "codes": codes, "indicators": "B600", "crc_ok": true,
               "level_db": 19.8, "floor_db": -80.0})
    }

    #[test]
    fn a_unit_gives_a_starting_state_then_only_its_changes() {
        let mut tr = Tracker::new(NS);
        let first = tr.add(&wiu("WIU", "1234560FF0", 1000.0, &[3, 15])).unwrap();
        assert_eq!(first["first"], true);
        assert_eq!(first["previous_codes"], Value::Null);
        assert_eq!(first["wiu"], "1234560ff0");
        assert_eq!(first["indicators"], "b600");
        assert_eq!(first["level_db"], -60.2);
        assert_eq!(first["source"], "status");
        // repeats are not sent
        assert!(tr.add(&wiu("WIU", "1234560ff0", 1004.0, &[3, 15])).is_none());
        let change = tr.add(&wiu("WIU", "1234560ff0", 1008.123, &[8, 15])).unwrap();
        assert_eq!(change["first"], false);
        assert_eq!(change["previous_codes"], json!([3, 15]));
        assert_eq!(change["codes"], json!([8, 15]));
        assert_eq!(change["t"], 1008.123);
        // the same record has the same ID
        let again = Tracker::new(NS).add(&wiu("WIU", "1234560ff0", 1008.123, &[8, 15])).unwrap();
        let want = Uuid::new_v5(&NS, b"itc-wayside:1234560ff0:1008123:8,15").to_string();
        assert_eq!(again["id"], json!(want));
        assert_eq!(change["id"], json!(want));
        // a short change and back is two records
        assert!(tr.add(&wiu("WIU", "1234560ff0", 1009.0, &[3, 15])).is_some());
        assert!(tr.add(&wiu("WIU", "1234560ff0", 1009.5, &[8, 15])).is_some());
        // after a gap of a minute, a starting state again
        let after = tr.add(&wiu("WIU", "1234560ff0", 1070.0, &[8, 15])).unwrap();
        assert_eq!(after["first"], true);
    }

    #[test]
    fn replies_count_only_while_the_unit_itself_is_quiet() {
        let mut tr = Tracker::new(NS);
        tr.add(&wiu("WIU", "1234560ff0", 1000.0, &[3])).unwrap();
        // a late reply with the old state while status messages come in: ignored
        tr.add(&wiu("WIU", "1234560ff0", 1004.0, &[8])).unwrap();
        assert!(tr.add(&wiu("WIU_REPLY", "1234560ff0", 1005.0, &[3])).is_none());
        // the unit's own messages stop; replies now say its state
        let r = tr.add(&wiu("WIU_REPLY", "1234560ff0", 1030.0, &[13])).unwrap();
        assert_eq!(r["source"], "reply");
        assert_eq!(r["previous_codes"], json!([8]));
        // a unit only heard through replies
        let other = tr.add(&wiu("WIU_REPLY", "1234560ff1", 1030.0, &[3])).unwrap();
        assert!(other["first"] == true && other["source"] == "reply");
        assert!(tr.add(&wiu("WIU_REPLY", "1234560ff1", 1034.0, &[8])).is_some());
    }

    #[test]
    fn bad_messages_make_no_record() {
        let mut tr = Tracker::new(NS);
        for m in [
            json!({"type": "WIU", "wiu": "12345", "t": 1.0, "ch": 114, "codes": [3]}),
            json!({"type": "WIU", "wiu": "1234560ff0", "t": 1.0, "ch": 114, "codes": []}),
            json!({"type": "WIU", "wiu": "1234560ff0", "t": 1.0, "ch": 114, "codes": [32]}),
            json!({"type": "WIU", "wiu": "1234560ff0", "t": 1.0, "ch": 0, "codes": [3]}),
            json!({"type": "WIU", "wiu": "1234560ff0", "t": 1.0, "ch": 114, "codes": [3], "crc_ok": false}),
            json!({"type": "TIME", "radio": "123456", "t": 1.0, "ch": 102}),
        ] {
            assert!(tr.add(&m).is_none(), "{m}");
        }
    }

    #[test]
    fn a_base_is_sent_when_first_heard_and_when_it_changes() {
        let mut tr = Tracker::new(NS);
        let beacon = |t: f64, local: i64| json!({"type": "BEACON", "radio": "123456", "t": t, "ch": 102, "lat": 40.5, "lon": -90.25,
                                                  "channel": local, "channels": [101, 127], "util": 20, "level_db": 12.0, "floor_db": -82.0});
        let b = tr.add(&beacon(1000.0, 114)).unwrap();
        assert_eq!(b, json!({"id": Uuid::new_v5(&NS, b"itc-base:123456:1000000").to_string(), "type": "base", "base": "123456",
                             "t": 1000.0, "lat": 40.5, "lon": -90.25, "local_channel": 114, "neighbour_channels": [101, 127],
                             "channel": 102, "level_db": -70.0}));
        assert!(tr.add(&beacon(1010.0, 114)).is_none());
        assert_eq!(tr.add(&beacon(1020.0, 116)).unwrap()["local_channel"], 116);
    }
}
