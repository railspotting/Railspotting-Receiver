//! Locomotive radios heard, as `loco` records for the headend (contract v1.21, section 2). No I/O.
//!
//! Locomotives check in with their base station (QSTAT) and acknowledge it (ACK); the base sends
//! them unicasts. Each names the locomotive's radio. A radio is taken for a locomotive's when it is
//! neither a base station (heard in a beacon) nor in a base's list of wayside radios (T07). Each
//! radio's messages make a sighting, sent when the radio has been quiet for `gap_s`, or every
//! hour while it is still heard. Only the locomotive's own transmissions set its peak: a unicast
//! is the base transmitting.

use crate::changes::{level, ms};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

/// A sighting still heard after this long is sent, and a new one starts.
pub const MAX_SIGHTING_S: f64 = 3600.0;

#[derive(Clone, Debug, PartialEq)]
struct Sighting {
    first: f64,
    last: f64,
    /// The strongest of its own transmissions (time, level, channel), else of any message.
    peak: Option<(f64, f64, i64)>,
    any_peak: (f64, f64, i64),
    messages: u64,
    base: Option<String>,
}

/// A message from a locomotive's own radio: (radio, time, level).
pub type Heard = (String, f64, f64);

pub struct Locos {
    ns: Uuid,
    gap_s: f64,
    sightings: BTreeMap<String, Sighting>,
    bases: BTreeSet<String>,
    waysides: BTreeSet<String>,
    /// Own transmissions in the last minute, for the heartbeat.
    recent: Vec<(f64, String)>,
}

fn radio_id(s: &str) -> Option<String> {
    let s = s.to_ascii_lowercase();
    (s.len() == 6 && s.chars().all(|c| c.is_ascii_hexdigit())).then_some(s)
}

/// The radios in a base's wayside list (T07): after a 9-byte header, 5-byte entries of a class
/// byte, the radio (3 bytes) and a status byte. The decoder keeps the last 4 bytes as `trailer`.
fn wayside_list(m: &Value) -> Vec<String> {
    let hex = format!("{}{}", m["hex"].as_str().unwrap_or(""), m["trailer"].as_str().unwrap_or(""));
    let Some(b) = (0..hex.len() / 2).map(|i| u8::from_str_radix(hex.get(2 * i..2 * i + 2)?, 16).ok()).collect::<Option<Vec<u8>>>() else {
        return vec![];
    };
    if b.len() < 9 || b[6] != 0x72 {
        return vec![];
    }
    let n = b[7] as usize;
    (0..n).filter_map(|k| b.get(9 + 5 * k + 1..9 + 5 * k + 4)).map(|r| r.iter().map(|x| format!("{x:02x}")).collect()).collect()
}

impl Locos {
    /// Record IDs are UUID v5 under `ns`; a radio quiet for `gap_s` ends its sighting.
    pub fn new(ns: Uuid, gap_s: f64) -> Locos {
        Locos { ns, gap_s, sightings: BTreeMap::new(), bases: BTreeSet::new(), waysides: BTreeSet::new(), recent: vec![] }
    }

    /// Record IDs are UUID v5 under `ns` (the receiver's ITC namespace).
    pub fn set_ns(&mut self, ns: Uuid) {
        self.ns = ns;
    }

    pub fn set_gap(&mut self, gap_s: f64) {
        self.gap_s = gap_s.clamp(60.0, 86400.0);
    }

    fn is_loco(&self, radio: &str) -> bool {
        !self.bases.contains(radio) && !self.waysides.contains(radio)
    }

    /// One decoded message. Returns the locomotive transmission it was, if it was one.
    pub fn add(&mut self, m: &Value) -> Option<Heard> {
        if m["crc_ok"] == false {
            return None;
        }
        let t = m["t"].as_f64()?;
        let kind = m["type"].as_str()?;
        let hex = m["hex"].as_str().unwrap_or("");
        let id_at = |i: usize| hex.get(i..i + 6).and_then(radio_id);
        let (radio, base, own) = match kind {
            "BEACON" => {
                if let Some(r) = m["radio"].as_str().and_then(radio_id) {
                    self.sightings.remove(&r);
                    self.bases.insert(r);
                }
                return None;
            }
            "T07" => {
                for r in wayside_list(m) {
                    self.sightings.remove(&r);
                    self.waysides.insert(r);
                }
                return None;
            }
            // sender, then the base it talks to
            "QSTAT" | "ACK" => (id_at(0)?, id_at(6), true),
            // a base to a radio
            "UNICAST" => {
                let from = id_at(0)?;
                if !self.bases.contains(&from) {
                    return None;
                }
                (id_at(6)?, Some(from), false)
            }
            _ => return None,
        };
        if !self.is_loco(&radio) || base.as_deref() == Some(radio.as_str()) {
            return None;
        }
        let lv = level(m);
        let ch = m["ch"].as_i64().filter(|c| (1..=999).contains(c)).unwrap_or(0);
        let s = self.sightings.entry(radio.clone()).or_insert(Sighting {
            first: t, last: t, peak: None, any_peak: (t, lv, ch), messages: 0, base: None,
        });
        s.first = s.first.min(t);
        s.last = s.last.max(t);
        s.messages += 1;
        if lv > s.any_peak.1 {
            s.any_peak = (t, lv, ch);
        }
        if own && s.peak.is_none_or(|p| lv > p.1) {
            s.peak = Some((t, lv, ch));
        }
        if base.is_some() {
            s.base = base;
        }
        if own {
            self.recent.retain(|(x, _)| *x >= t - 3600.0);
            self.recent.push((t, radio.clone()));
            return Some((radio, t, lv));
        }
        None
    }

    /// Distinct locomotive radios that transmitted between `now - window_s` and `now`.
    pub fn heard(&self, now: f64, window_s: f64) -> usize {
        self.recent.iter().filter(|(t, _)| *t >= now - window_s).map(|(_, r)| r).collect::<BTreeSet<_>>().len()
    }

    /// The records due at `now`: sightings quiet for `gap_s`, or running for an hour.
    pub fn due(&mut self, now: f64) -> Vec<Value> {
        let mut out = vec![];
        let done: Vec<String> = self.sightings.iter()
            .filter(|(_, s)| now - s.last > self.gap_s || s.last - s.first >= MAX_SIGHTING_S)
            .map(|(r, _)| r.clone()).collect();
        for r in done {
            if let Some(s) = self.sightings.remove(&r) {
                out.push(self.record(&r, &s, now));
            }
        }
        out
    }

    fn record(&self, radio: &str, s: &Sighting, t: f64) -> Value {
        let (pt, pl, ch) = s.peak.unwrap_or(s.any_peak);
        let id = Uuid::new_v5(&self.ns, format!("itc-loco:{radio}:{}", ms(s.first)).as_bytes());
        // the headend wants first <= peak_t <= last <= t
        let round = |x: f64| ms(x) as f64 / 1000.0;
        let (first, last) = (round(s.first), round(s.last));
        json!({
            "id": id.to_string(), "type": "loco", "radio": radio,
            "base": s.base.as_deref().and_then(radio_id),
            "t": round(t.max(s.last)), "first": first, "last": last,
            "peak_t": round(pt).clamp(first, last), "peak_level_db": pl,
            "messages": s.messages.clamp(1, 100_000),
            "channel": if ch > 0 { json!(ch) } else { Value::Null },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NS: Uuid = uuid::uuid!("6f1c4e1e-6a0b-4a52-9d55-4d3c1f0a7b15");

    fn msg(kind: &str, t: f64, hex: &str, level: f64) -> Value {
        json!({"type": kind, "t": t, "ch": 114, "hex": hex, "radio": &hex[..6], "crc_ok": true,
               "level_db": level, "floor_db": -40.0})
    }

    fn setup() -> Locos {
        let mut l = Locos::new(NS, 1800.0);
        l.add(&json!({"type": "BEACON", "radio": "123456", "t": 0.0, "crc_ok": true}));
        // the base lists wayside radios 3c0001 and 3c0002 (the second entry ends in the trailer)
        l.add(&json!({"type": "T07", "t": 1.0, "crc_ok": true, "radio": "123456",
                      "hex": "1234560a0000720204" .to_string() + "1a3c000101" + "1b", "trailer": "3c000203"}));
        l
    }

    #[test]
    fn check_ins_from_a_locomotive_make_one_sighting_record() {
        let mut l = setup();
        assert_eq!(l.add(&msg("QSTAT", 100.0, "a1b2c312345600002d", 10.0)), Some(("a1b2c3".into(), 100.0, -30.0)));
        l.add(&msg("QSTAT", 145.0, "a1b2c312345600002d", 25.0));
        // the base's unicast to it counts, but its level is the base's, not the locomotive's
        assert_eq!(l.add(&msg("UNICAST", 150.0, "123456a1b2c37c0023", 39.0)), None);
        l.add(&msg("QSTAT", 190.0, "a1b2c312345600002d", 12.0));
        assert_eq!(l.heard(200.0, 60.0), 1);
        assert!(l.due(1000.0).is_empty(), "still within the gap");
        let r = l.due(190.0 + 1801.0);
        assert_eq!(r.len(), 1);
        let r = &r[0];
        assert_eq!(r["type"], "loco");
        assert_eq!(r["radio"], "a1b2c3");
        assert_eq!(r["base"], "123456");
        assert_eq!((r["first"].as_f64(), r["last"].as_f64()), (Some(100.0), Some(190.0)));
        assert_eq!((r["peak_t"].as_f64(), r["peak_level_db"].as_f64()), (Some(145.0), Some(-15.0)));
        assert_eq!(r["messages"], 4);
        assert_eq!(r["id"], Uuid::new_v5(&NS, b"itc-loco:a1b2c3:100000").to_string());
        assert!(r["t"].as_f64().unwrap() >= r["last"].as_f64().unwrap());
    }

    #[test]
    fn bases_and_listed_waysides_are_not_locomotives() {
        let mut l = setup();
        assert_eq!(l.add(&msg("QSTAT", 100.0, "3c000112345600002d", 20.0)), None);
        assert_eq!(l.add(&msg("QSTAT", 100.0, "3c000212345600002d", 20.0)), None);
        assert_eq!(l.add(&msg("QSTAT", 100.0, "12345612345600002d", 20.0)), None);
        // a unicast from something not known as a base names nobody
        assert_eq!(l.add(&msg("UNICAST", 100.0, "999999a1b2c37c0023", 20.0)), None);
        assert!(l.due(1e6).is_empty());
    }

    #[test]
    fn a_long_sighting_is_sent_every_hour_and_a_bad_base_is_null() {
        let mut l = setup();
        for k in 0..100 {
            l.add(&msg("QSTAT", 100.0 + k as f64 * 45.0, "a1b2c3zzzzzz00002d", 10.0));
        }
        let r = l.due(100.0 + 99.0 * 45.0);
        assert_eq!(r.len(), 1, "over an hour: sent while still heard");
        assert!(r[0]["base"].is_null());
        assert!(r[0]["last"].as_f64().unwrap() - r[0]["first"].as_f64().unwrap() >= MAX_SIGHTING_S);
    }

    #[test]
    fn the_wayside_list_is_read_with_its_trailer() {
        let m = json!({"hex": "1234560a0000720204".to_string() + "1a3c000101" + "1b", "trailer": "3c000203"});
        assert_eq!(wayside_list(&m), vec!["3c0001", "3c0002"]);
        assert!(wayside_list(&json!({"hex": "1234"})).is_empty());
    }
}
