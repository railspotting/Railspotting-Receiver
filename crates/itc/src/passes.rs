//! Trains passing the receiver, as `pass` records for the headend (contract v1.21, section 3). No
//! I/O.
//!
//! A pass starts when a wayside drops from proceed to stop. It gathers the drops that follow, the
//! locomotive radios transmitting around it, the end-of-train unit heard and the detector event
//! sent for the train, and ends after `QUIET_S` with no drop and no message from its own
//! locomotives. Its locomotives are the radios that peak within `LOCO_DB` of the loudest, so a
//! train elsewhere on the same base, heard faintly, is left out. `closest` is when the loudest
//! was loudest: the train nearest the receiver.

use crate::changes::ms;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use uuid::Uuid;

/// A pass ends after this long with no drop and no message from its locomotives.
pub const QUIET_S: f64 = 600.0;
/// Locomotive messages and EOT bursts this long before the first drop count for the pass.
pub const BEFORE_S: f64 = 600.0;
/// Radios peaking within this many dB of the loudest are the train's.
pub const LOCO_DB: f64 = 8.0;
pub const MAX_LOCOS: usize = 8;
pub const MAX_DROPS: usize = 64;
/// End-of-train unit IDs are 17 bits.
pub const EOT_MAX: i64 = 131_071;

#[derive(Default)]
struct Pass {
    first: f64,
    active: f64,
    drops: Vec<(String, f64)>,
    /// radio: (peak time, peak level, last message)
    locos: BTreeMap<String, (f64, f64, f64)>,
    eot: BTreeMap<i64, u32>,
    detector: Option<String>,
}

pub struct Passes {
    ns: Uuid,
    cur: Option<Pass>,
    /// Recent locomotive messages and EOT units, for a pass that starts after them.
    before_locos: Vec<(String, f64, f64)>,
    before_eot: Vec<(i64, f64)>,
}

fn uuid_ok(s: &str) -> bool {
    Uuid::parse_str(s).is_ok_and(|u| u.hyphenated().to_string() == s)
}

impl Passes {
    pub fn new(ns: Uuid) -> Passes {
        Passes { ns, cur: None, before_locos: vec![], before_eot: vec![] }
    }

    /// Record IDs are UUID v5 under `ns` (the receiver's ITC namespace).
    pub fn set_ns(&mut self, ns: Uuid) {
        self.ns = ns;
    }

    /// A wayside dropped from proceed to stop.
    pub fn drop(&mut self, wiu: &str, t: f64) {
        let p = self.cur.get_or_insert_with(|| Pass { first: t, active: t, ..Default::default() });
        if p.drops.len() < MAX_DROPS && !p.drops.iter().any(|(w, _)| w == wiu) {
            p.drops.push((wiu.to_string(), t));
        }
        p.active = p.active.max(t);
        if p.drops.len() == 1 {
            // what came just before the first drop is the same train approaching
            let since = t - BEFORE_S;
            for (r, tt, lv) in std::mem::take(&mut self.before_locos).into_iter().filter(|x| x.1 >= since) {
                Self::loco_into(p, &r, tt, lv, false);
            }
            for (u, tt) in std::mem::take(&mut self.before_eot).into_iter().filter(|x| x.1 >= since) {
                *p.eot.entry(u).or_default() += 1;
                let _ = tt;
            }
        }
    }

    fn loco_into(p: &mut Pass, radio: &str, t: f64, level: f64, keeps_alive: bool) {
        let e = p.locos.entry(radio.to_string()).or_insert((t, level, t));
        if level > e.1 {
            (e.0, e.1) = (t, level);
        }
        e.2 = e.2.max(t);
        if keeps_alive {
            p.active = p.active.max(t);
        }
    }

    /// A locomotive radio's own transmission.
    pub fn loco(&mut self, radio: &str, t: f64, level: f64) {
        match self.cur.as_mut() {
            Some(p) => {
                // a radio already in the pass keeps it going; a new one only joins it
                let known = p.locos.contains_key(radio);
                Self::loco_into(p, radio, t, level, known);
            }
            None => {
                self.before_locos.push((radio.to_string(), t, level));
                self.before_locos.retain(|x| x.1 >= t - BEFORE_S);
            }
        }
    }

    /// An end-of-train unit heard.
    pub fn eot(&mut self, unit: i64, t: f64) {
        if !(0..=EOT_MAX).contains(&unit) {
            return;
        }
        match self.cur.as_mut() {
            Some(p) => *p.eot.entry(unit).or_default() += 1,
            None => {
                self.before_eot.push((unit, t));
                self.before_eot.retain(|x| x.1 >= t - BEFORE_S);
            }
        }
    }

    /// A detector event this receiver sent (its `event_id`), not a repeat.
    pub fn detector(&mut self, event_id: &str, _t: f64) {
        if let Some(p) = self.cur.as_mut() {
            if p.detector.is_none() && uuid_ok(event_id) {
                p.detector = Some(event_id.to_string());
            }
        }
    }

    /// The pass that has ended by `now`, as a record.
    pub fn due(&mut self, now: f64) -> Option<Value> {
        if self.cur.as_ref().is_none_or(|p| now - p.active <= QUIET_S) {
            return None;
        }
        let p = self.cur.take()?;
        Some(self.record(&p, now))
    }

    fn record(&self, p: &Pass, t: f64) -> Value {
        let round = |x: f64| ms(x) as f64 / 1000.0;
        let mut locos: Vec<(&String, &(f64, f64, f64))> = p.locos.iter().collect();
        locos.sort_by(|a, b| b.1 .1.total_cmp(&a.1 .1).then(a.0.cmp(b.0)));
        let loudest = locos.first().map(|l| l.1 .1);
        locos.retain(|l| loudest.is_some_and(|top| l.1 .1 >= top - LOCO_DB));
        locos.truncate(MAX_LOCOS);
        let closest = locos.first().map(|l| round(l.1 .0.min(t)));
        let anchor = closest.unwrap_or(p.first);
        let eot = p.eot.iter().max_by_key(|(u, n)| (**n, -**u)).map(|(u, _)| *u);
        json!({
            "id": Uuid::new_v5(&self.ns, format!("itc-pass:{}", ms(anchor)).as_bytes()).to_string(),
            "type": "pass", "t": round(t), "closest": closest,
            "locos": locos.iter().map(|(r, (pt, pl, _))| json!({"radio": r, "peak_t": round(*pt), "peak_level_db": pl})).collect::<Vec<_>>(),
            "drops": p.drops.iter().map(|(w, dt)| json!({"wiu": w, "t": round(*dt)})).collect::<Vec<_>>(),
            "eot_unit": eot, "detector_event_id": p.detector,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NS: Uuid = uuid::uuid!("6f1c4e1e-6a0b-4a52-9d55-4d3c1f0a7b15");
    const DET: &str = "0b7a3c52-3f6e-4d2a-9b1c-8e5f7a6d4c3b";

    #[test]
    fn a_pass_joins_drops_locomotives_eot_and_detector() {
        let mut p = Passes::new(NS);
        // the train's locomotives are heard as it approaches, before any signal drops
        p.loco("a1b2c3", 1000.0, -40.0);
        p.loco("a1b2c4", 1010.0, -42.0);
        p.loco("ffff01", 1020.0, -70.0); // another train, far off
        p.drop("1234560101", 1200.0);
        p.loco("a1b2c3", 1300.0, -20.0);
        p.loco("a1b2c4", 1310.0, -24.0);
        p.drop("1234560103", 1305.0);
        p.drop("1234560101", 1306.0); // the same unit again: listed once
        p.eot(67000, 1500.0);
        p.eot(67000, 1510.0);
        p.detector(DET, 1520.0);
        p.detector("not-a-uuid", 1521.0);
        assert!(p.due(1310.0 + QUIET_S).is_none(), "not quiet long enough");
        let r = p.due(1310.0 + QUIET_S + 1.0).expect("the pass ended");
        assert_eq!(r["type"], "pass");
        assert_eq!(r["closest"], 1300.0);
        assert_eq!(r["id"], Uuid::new_v5(&NS, b"itc-pass:1300000").to_string());
        let locos: Vec<&str> = r["locos"].as_array().unwrap().iter().map(|l| l["radio"].as_str().unwrap()).collect();
        assert_eq!(locos, vec!["a1b2c3", "a1b2c4"], "strongest first; the faint one left out");
        let drops: Vec<&str> = r["drops"].as_array().unwrap().iter().map(|d| d["wiu"].as_str().unwrap()).collect();
        assert_eq!(drops, vec!["1234560101", "1234560103"]);
        assert_eq!(r["eot_unit"], 67000);
        assert_eq!(r["detector_event_id"], DET);
        assert!(r["closest"].as_f64().unwrap() <= r["t"].as_f64().unwrap());
        assert!(p.due(1e9).is_none());
    }

    #[test]
    fn limits_and_a_pass_with_no_locomotive() {
        let mut p = Passes::new(NS);
        for k in 0..100 {
            p.drop(&format!("1234560{:03}", k), 100.0 + k as f64);
        }
        for k in 0..20 {
            p.loco(&format!("a1b2{:02x}", k), 150.0, -30.0 - (k % 3) as f64);
        }
        p.eot(EOT_MAX + 1, 160.0);
        let r = p.due(1000.0).unwrap();
        assert_eq!(r["drops"].as_array().unwrap().len(), MAX_DROPS);
        assert_eq!(r["locos"].as_array().unwrap().len(), MAX_LOCOS);
        assert!(r["eot_unit"].is_null());
        assert!(r["detector_event_id"].is_null());
        let mut q = Passes::new(NS);
        q.drop("1234560101", 500.0);
        let r = q.due(2000.0).unwrap();
        assert!(r["closest"].is_null());
        assert_eq!(r["id"], Uuid::new_v5(&NS, b"itc-pass:500000").to_string());
    }
}
