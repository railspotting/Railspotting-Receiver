//! Trains passing the receiver, as `pass` records for the headend (contract v1.21, section 3). No
//! I/O.
//!
//! A pass is built around the train's locomotives, not around signal drops: on a busy line
//! waysides drop every couple of minutes (trains, and dispatchers setting and cancelling routes),
//! so drops alone never leave a quiet gap to end a pass. Locomotive radios that start
//! transmitting within `GROUP_S` of each other are one train. Its pass ends when they have all been
//! quiet for `QUIET_S`. `closest` is when the loudest of them was loudest: the train nearest the
//! receiver. The pass gathers the drops, end-of-train units and detector events from the train's
//! time window, and is kept only when a wayside dropped within `NEAR_S` of `closest`. On a busy
//! line that rarely leaves a distant train out (something drops every few minutes); the headend
//! links a pass to its train by its EOT unit, detector event and time.

use crate::changes::ms;
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use uuid::Uuid;

/// A train's pass ends after its radios have been quiet this long.
pub const QUIET_S: f64 = 600.0;
/// Radios that start transmitting this close together are one train.
pub const GROUP_S: f64 = 180.0;
/// Drops, EOT units and detector events this long either side of the train's radios count.
pub const WINDOW_S: f64 = 600.0;
/// A pass is kept only with a drop this close to `closest`.
pub const NEAR_S: f64 = 300.0;
/// Radios peaking within this many dB of the loudest are listed.
pub const LOCO_DB: f64 = 8.0;
pub const MAX_LOCOS: usize = 8;
pub const MAX_DROPS: usize = 64;
/// End-of-train unit IDs are 17 bits.
pub const EOT_MAX: i64 = 131_071;
/// Drops, EOT units and detector events are kept this long, for passes still open.
const KEEP_S: f64 = 3.0 * 3600.0;

/// A radio in a train: (peak time, peak level, first message, last message).
type Radio = (f64, f64, f64, f64);

/// One train's locomotive radios.
#[derive(Default)]
struct Train {
    radios: BTreeMap<String, Radio>,
    first: f64,
    last: f64,
}

pub struct Passes {
    ns: Uuid,
    trains: Vec<Train>,
    drops: VecDeque<(String, f64)>,
    eots: VecDeque<(i64, f64)>,
    detectors: VecDeque<(String, f64)>,
}

fn uuid_ok(s: &str) -> bool {
    Uuid::parse_str(s).is_ok_and(|u| u.hyphenated().to_string() == s)
}

impl Passes {
    pub fn new(ns: Uuid) -> Passes {
        Passes { ns, trains: vec![], drops: VecDeque::new(), eots: VecDeque::new(), detectors: VecDeque::new() }
    }

    /// Record IDs are UUID v5 under `ns` (the receiver's ITC namespace).
    pub fn set_ns(&mut self, ns: Uuid) {
        self.ns = ns;
    }

    fn prune(&mut self, t: f64) {
        while self.drops.front().is_some_and(|d| d.1 < t - KEEP_S) {
            self.drops.pop_front();
        }
        while self.eots.front().is_some_and(|d| d.1 < t - KEEP_S) {
            self.eots.pop_front();
        }
        while self.detectors.front().is_some_and(|d| d.1 < t - KEEP_S) {
            self.detectors.pop_front();
        }
    }

    /// A wayside dropped from proceed to stop.
    pub fn drop(&mut self, wiu: &str, t: f64) {
        self.drops.push_back((wiu.to_string(), t));
        self.prune(t);
    }

    /// A locomotive radio's own transmission.
    pub fn loco(&mut self, radio: &str, t: f64, level: f64) {
        // its train: the one it is already in, else one whose radios started just now
        let k = self.trains.iter().position(|tr| tr.radios.contains_key(radio))
            .or_else(|| self.trains.iter().position(|tr| (t - tr.first).abs() <= GROUP_S));
        let tr = match k {
            Some(k) => &mut self.trains[k],
            None => {
                self.trains.push(Train { first: t, last: t, ..Default::default() });
                let n = self.trains.len() - 1;
                &mut self.trains[n]
            }
        };
        let e = tr.radios.entry(radio.to_string()).or_insert((t, level, t, t));
        if level > e.1 {
            (e.0, e.1) = (t, level);
        }
        e.3 = e.3.max(t);
        tr.first = tr.first.min(t);
        tr.last = tr.last.max(t);
    }

    /// An end-of-train unit heard.
    pub fn eot(&mut self, unit: i64, t: f64) {
        if (0..=EOT_MAX).contains(&unit) {
            self.eots.push_back((unit, t));
            self.prune(t);
        }
    }

    /// A detector event this receiver sent (its `event_id`), not a repeat.
    pub fn detector(&mut self, event_id: &str, t: f64) {
        if uuid_ok(event_id) {
            self.detectors.push_back((event_id.to_string(), t));
            self.prune(t);
        }
    }

    /// The passes that have ended by `now`, as records.
    pub fn due(&mut self, now: f64) -> Vec<Value> {
        let (done, open): (Vec<Train>, Vec<Train>) = std::mem::take(&mut self.trains).into_iter().partition(|tr| now - tr.last > QUIET_S);
        self.trains = open;
        done.iter().filter_map(|tr| self.record(tr, now)).collect()
    }

    fn record(&self, tr: &Train, t: f64) -> Option<Value> {
        let round = |x: f64| ms(x) as f64 / 1000.0;
        let mut locos: Vec<(&String, &Radio)> = tr.radios.iter().collect();
        locos.sort_by(|a, b| b.1 .1.total_cmp(&a.1 .1).then(a.0.cmp(b.0)));
        let top = locos.first()?.1;
        let closest = top.0.min(t);
        locos.retain(|l| l.1 .1 >= top.1 - LOCO_DB);
        locos.truncate(MAX_LOCOS);
        let (from, to) = (tr.first - WINDOW_S, tr.last + WINDOW_S);
        let mut drops: Vec<&(String, f64)> = vec![];
        for d in self.drops.iter().filter(|d| d.1 >= from && d.1 <= to) {
            if drops.len() < MAX_DROPS && !drops.iter().any(|x| x.0 == d.0) {
                drops.push(d);
            }
        }
        // a train heard faintly elsewhere on the base drops nothing near its closest approach
        if !drops.iter().any(|d| (d.1 - closest).abs() <= NEAR_S) {
            return None;
        }
        let mut count: BTreeMap<i64, u32> = BTreeMap::new();
        for (u, _) in self.eots.iter().filter(|e| e.1 >= from && e.1 <= to) {
            *count.entry(*u).or_default() += 1;
        }
        let eot = count.iter().max_by_key(|(u, n)| (**n, -**u)).map(|(u, _)| *u);
        let detector = self.detectors.iter().find(|d| d.1 >= from && d.1 <= to).map(|d| d.0.clone());
        Some(json!({
            "id": Uuid::new_v5(&self.ns, format!("itc-pass:{}", ms(closest)).as_bytes()).to_string(),
            "type": "pass", "t": round(t), "closest": round(closest),
            "locos": locos.iter().map(|(r, (pt, pl, _, _))| json!({"radio": r, "peak_t": round(*pt), "peak_level_db": pl})).collect::<Vec<_>>(),
            "drops": drops.iter().map(|(w, dt)| json!({"wiu": w, "t": round(*dt)})).collect::<Vec<_>>(),
            "eot_unit": eot, "detector_event_id": detector,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NS: Uuid = uuid::uuid!("6f1c4e1e-6a0b-4a52-9d55-4d3c1f0a7b15");
    const DET: &str = "0b7a3c52-3f6e-4d2a-9b1c-8e5f7a6d4c3b";

    #[test]
    fn a_pass_is_the_trains_radios_with_its_drops_eot_and_detector() {
        let mut p = Passes::new(NS);
        // a busy line: drops all the time, from routes being set and cancelled too
        for k in 0..40 {
            p.drop(&format!("12345602{:02}", k % 7), k as f64 * 100.0);
        }
        // a train: two radios starting together, loudest at 2000
        p.loco("a1b2c3", 1700.0, -40.0);
        p.loco("a1b2c4", 1760.0, -42.0);
        p.loco("a1b2c3", 2000.0, -20.0);
        p.loco("a1b2c4", 2010.0, -24.0);
        p.drop("1234560101", 2005.0);
        p.drop("1234560103", 2100.0);
        p.loco("a1b2c3", 2200.0, -30.0);
        p.eot(67000, 2150.0);
        p.eot(67000, 2160.0);
        p.detector(DET, 2300.0);
        p.detector("not-a-uuid", 2301.0);
        // another train, much later, after the line has gone quiet
        p.loco("ffff01", 6000.0, -70.0);
        assert!(p.due(2200.0 + QUIET_S).is_empty(), "not quiet long enough");
        let got = p.due(2200.0 + QUIET_S + 1.0);
        assert_eq!(got.len(), 1);
        let r = &got[0];
        assert_eq!(r["closest"], 2000.0);
        assert_eq!(r["id"], Uuid::new_v5(&NS, b"itc-pass:2000000").to_string());
        let locos: Vec<&str> = r["locos"].as_array().unwrap().iter().map(|l| l["radio"].as_str().unwrap()).collect();
        assert_eq!(locos, vec!["a1b2c3", "a1b2c4"]);
        let drops: Vec<&str> = r["drops"].as_array().unwrap().iter().map(|d| d["wiu"].as_str().unwrap()).collect();
        assert!(drops.contains(&"1234560101") && drops.contains(&"1234560103"), "{drops:?}");
        let times: Vec<f64> = r["drops"].as_array().unwrap().iter().map(|d| d["t"].as_f64().unwrap()).collect();
        assert!(times.windows(2).all(|w| w[0] <= w[1]), "in the order they happened");
        assert_eq!(r["eot_unit"], 67000);
        assert_eq!(r["detector_event_id"], DET);
        // it ends too, but no wayside dropped near its closest approach: no pass
        p.drop("1234560105", 6400.0);
        assert!(p.due(6000.0 + QUIET_S + 1.0).is_empty());
    }

    #[test]
    fn limits_and_no_pass_without_a_drop() {
        let mut p = Passes::new(NS);
        for k in 0..100 {
            p.drop(&format!("1234560{:03}", k), 100.0 + k as f64);
        }
        for k in 0..20 {
            p.loco(&format!("a1b2{:02x}", k), 150.0, -30.0 - (k % 3) as f64);
        }
        p.eot(EOT_MAX + 1, 160.0);
        let r = &p.due(1000.0)[0];
        assert_eq!(r["drops"].as_array().unwrap().len(), MAX_DROPS);
        assert_eq!(r["locos"].as_array().unwrap().len(), MAX_LOCOS);
        assert!(r["eot_unit"].is_null());
        assert!(r["detector_event_id"].is_null());
        let mut q = Passes::new(NS);
        q.loco("a1b2c3", 500.0, -20.0);
        assert!(q.due(2000.0).is_empty(), "no wayside dropped: not a pass here");
    }
}
