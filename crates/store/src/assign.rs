//! Automatic dongle roles: which connected RTL-SDR does which job.
//!
//! When receiver.json names no dongle serials, each role (VHF voice, UHF EOT, ...) takes a connected
//! dongle by itself. The choice is arbitrary but stable: it is saved in state/sdr_roles.json, and a
//! role moves to another dongle only after its own has been missing for a while. The owner then
//! connects the right antenna to each dongle; the log, `status` and the status page say which.
//!
//! The headend can choose instead (contract v1.11, `sdr_roles` in /config): the owner picks a job
//! for each dongle on the website. A dongle it names takes that role (or none, for "off"); the
//! others still fill the free roles by themselves.
//!
//! The receiver supplies the dongle listing (sysfs or librtlsdr); this module only sees serials.

use crate::jsonfmt;
use railspotting_common::{Clock, PLACEHOLDER_SERIALS};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

pub const ROLES_FILE: &str = "sdr_roles.json";
pub const RELEASE_AFTER_S: f64 = 60.0; // a role waits this long for its missing dongle before moving on
/// The roles a receiver fills by itself, in order: the first dongle found takes EOT, the second
/// VHF (voice: detectors and road calls), the third HOT (an owner's decision, October 2026).
pub const DEFAULT_ROLES: [&str; 3] = ["eot", "vhf", "hot"];
pub const UHF_ANTENNA: &str = "the UHF antenna (the shorter one, or the diplexer's high-band port)";

/// Serials that mean "pick one for me".
pub fn is_auto_serial(s: &str) -> bool {
    PLACEHOLDER_SERIALS.contains(&s) || s == "auto" || s.is_empty()
}

/// The built-in settings for a role, in the key order sdr_roles.json has.
pub fn role_defaults(role: &str) -> Option<Value> {
    match role {
        "vhf" => Some(json!({"name": "VHF SDR (voice)", "gain": 20.7, "sample_rate": 2_880_000.0,
                             "antenna": "the VHF antenna (the longer one, or the diplexer's low-band port)"})),
        "eot" => Some(json!({"name": "UHF SDR (EOT)", "gain": 40.2, "sample_rate": 2_400_000.0,
                             "antenna": UHF_ANTENNA})),
        "hot" => Some(json!({"name": "UHF SDR (HOT)", "gain": 40.2, "sample_rate": 2_400_000.0,
                             "antenna": UHF_ANTENNA})),
        _ => None,
    }
}

/// The serial field as text: empty for null, false, zero and empty values, `True` for true.
fn value_str_or_empty(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) | Some(Value::Bool(false)) => String::new(),
        Some(Value::Bool(true)) => "True".into(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) if n.as_f64() == Some(0.0) => String::new(),
        Some(Value::Number(n)) => match n.as_f64() {
            Some(f) if !n.is_i64() && !n.is_u64() => crate::jsonfmt::float_repr(f),
            _ => n.to_string(),
        },
        Some(Value::Array(a)) if a.is_empty() => String::new(),
        Some(Value::Object(o)) if o.is_empty() => String::new(),
        Some(other) => other.to_string(),
    }
}

fn devices(sdr: &Value) -> Vec<Value> {
    sdr.get("devices").and_then(Value::as_array).cloned().unwrap_or_default()
}

/// True when receiver.json leaves the serials to the receiver.
pub fn auto_mode(sdr_settings: &Value) -> bool {
    if sdr_settings.get("auto_assign") == Some(&Value::Bool(false)) {
        return false;
    }
    devices(sdr_settings).iter().all(|d| is_auto_serial(&value_str_or_empty(d.get("serial"))))
}

/// One device config per role, with no serial yet; receiver.json entries override defaults.
pub fn role_configs(sdr_settings: &Value) -> Vec<Value> {
    let mut given = devices(sdr_settings);
    if given.is_empty() {
        given = DEFAULT_ROLES.iter().map(|r| json!({"role": r})).collect();
    }
    let mut out = Vec::new();
    for d in given {
        let role = d.get("role").and_then(Value::as_str).unwrap_or("vhf").to_string();
        let base = role_defaults(&role)
            .unwrap_or_else(|| json!({"name": format!("{} SDR", role.to_uppercase()), "antenna": UHF_ANTENNA}));
        let mut cfg = Map::new();
        cfg.insert("center_mhz".into(), json!("auto"));
        for (k, v) in base.as_object().into_iter().flatten() {
            cfg.insert(k.clone(), v.clone());
        }
        for (k, v) in d.as_object().into_iter().flatten() {
            if k != "serial" {
                cfg.insert(k.clone(), v.clone());
            }
        }
        cfg.insert("serial".into(), json!(""));
        out.push(Value::Object(cfg));
    }
    out
}

/// Unique keys for saving: the role, or role-2, role-3 when a role appears twice.
pub fn slot_keys<S: AsRef<str>>(roles: &[S]) -> Vec<String> {
    let mut seen: HashMap<&str, usize> = HashMap::new();
    roles
        .iter()
        .map(|r| {
            let r = r.as_ref();
            let n = seen.entry(r).or_insert(0);
            *n += 1;
            if *n == 1 { r.to_string() } else { format!("{r}-{n}") }
        })
        .collect()
}

/// (slot index, old serial, new serial)
pub type Change = (usize, Option<String>, String);

pub struct RoleAssigner {
    pub keys: Vec<String>,
    pub path: PathBuf,
    clock: Clock,
    pub release_after_s: f64,
    /// Serial per slot, in slot order.
    pub assigned: Vec<Option<String>>,
    missing_since: HashMap<usize, f64>,
    pub duplicates: Vec<String>,
    pub spare: Vec<String>,
    /// The headend's choices for plugged-in dongles: serial to role, or "off".
    pub chosen: BTreeMap<String, String>,
}

impl RoleAssigner {
    pub fn new<S: AsRef<str>>(roles: &[S], state_dir: &Path, clock: Clock) -> Self {
        Self::with_release(roles, state_dir, clock, RELEASE_AFTER_S)
    }

    pub fn with_release<S: AsRef<str>>(roles: &[S], state_dir: &Path, clock: Clock, release_after_s: f64) -> Self {
        let keys = slot_keys(roles);
        let path = state_dir.join(ROLES_FILE);
        let mut assigned = vec![None; keys.len()];
        if let Some(saved) = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
            for (i, k) in keys.iter().enumerate() {
                if let Some(s) = saved.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()) {
                    assigned[i] = Some(s.to_string());
                }
            }
        }
        RoleAssigner { keys, path, clock, release_after_s, assigned, missing_since: HashMap::new(),
                       duplicates: Vec::new(), spare: Vec::new(), chosen: BTreeMap::new() }
    }

    /// The serial assigned to a slot key.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.keys.iter().position(|k| k == key).and_then(|i| self.assigned[i].as_deref())
    }

    /// The saved file's contents: {slot key: serial or null}.
    pub fn as_json(&self) -> Value {
        let mut m = Map::new();
        for (k, s) in self.keys.iter().zip(&self.assigned) {
            m.insert(k.clone(), s.as_ref().map_or(Value::Null, |s| json!(s)));
        }
        Value::Object(m)
    }

    pub fn save(&self) -> std::io::Result<()> {
        let tmp = self.path.with_file_name(format!("{ROLES_FILE}.tmp"));
        std::fs::write(&tmp, jsonfmt::dumps_indent(&self.as_json(), 2) + "\n")?;
        std::fs::rename(tmp, &self.path)
    }

    /// A slot's role: its key without the "-2" of a second slot for the same role.
    pub fn role_of(&self, i: usize) -> &str {
        let k = &self.keys[i];
        match k.rsplit_once('-') {
            Some((r, n)) if n.parse::<usize>().is_ok() => r,
            _ => k,
        }
    }

    /// Match the connected serials to roles. A change with an empty new serial leaves the slot
    /// without a dongle (the headend gave its dongle another job, or none).
    pub fn update<S: AsRef<str>>(&mut self, serials: &[S]) -> Vec<Change> {
        let now = (self.clock)();
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for s in serials {
            *counts.entry(s.as_ref().to_string()).or_insert(0) += 1;
        }
        // dongles that share a serial (or have none) can't be told apart, so none of them is used
        let mut dups: Vec<String> = counts
            .iter()
            .filter(|(s, n)| **n > 1 || s.is_empty())
            .map(|(s, _)| if s.is_empty() { "(none)".to_string() } else { s.clone() })
            .collect();
        dups.sort();
        self.duplicates = dups;
        let present: Vec<String> = counts.iter().filter(|(s, n)| **n == 1 && !s.is_empty()).map(|(s, _)| s.clone()).collect();
        for i in 0..self.keys.len() {
            match &self.assigned[i] {
                Some(s) if present.contains(s) => {
                    self.missing_since.remove(&i);
                }
                Some(_) => {
                    self.missing_since.entry(i).or_insert(now);
                }
                None => {}
            }
        }
        let mut changes = Vec::new();
        // the headend's choices first, for the dongles it names that are plugged in
        let named: Vec<&String> = present.iter().filter(|s| self.chosen.contains_key(*s)).collect();
        let mut forced: Vec<Option<String>> = vec![None; self.keys.len()];
        for keep_place in [true, false] {
            for s in &named {
                let role = self.chosen[*s].as_str();
                if role == "off" || forced.iter().any(|f| f.as_ref() == Some(*s)) {
                    continue;
                }
                // a dongle already in a slot for its role stays in that slot
                let slot = (0..self.keys.len()).find(|&i| forced[i].is_none() && self.role_of(i) == role
                    && (!keep_place || self.assigned[i].as_ref() == Some(*s)));
                if let Some(i) = slot {
                    forced[i] = Some((*s).clone());
                }
            }
        }
        for (i, f) in forced.iter().enumerate() {
            let old = self.assigned[i].clone();
            match f {
                Some(s) if old.as_ref() != Some(s) => {
                    changes.push((i, old, s.clone()));
                    self.assigned[i] = Some(s.clone());
                    self.missing_since.remove(&i);
                }
                None if old.as_ref().is_some_and(|o| named.contains(&o)) => {
                    // its dongle has another job now (or none)
                    changes.push((i, old, String::new()));
                    self.assigned[i] = None;
                    self.missing_since.remove(&i);
                }
                _ => {}
            }
        }
        let mut free: Vec<String> = present
            .iter()
            .filter(|s| !self.chosen.contains_key(*s) && !self.assigned.iter().any(|a| a.as_ref() == Some(*s)))
            .cloned()
            .collect();
        // A job whose dongle is gone (thrown away, or replaced) gets the next free dongle before a
        // job that never had one, once its own has been missing for release_after_s (it may only
        // be being replugged). Until then, enough free dongles are kept back for it.
        let missing = |i: usize| self.assigned[i].as_ref().is_some_and(|old| !present.contains(old));
        let released = |i: usize| now - self.missing_since.get(&i).copied().unwrap_or(now) >= self.release_after_s;
        let replacing: Vec<usize> = (0..self.keys.len()).filter(|&i| missing(i) && released(i)).collect();
        let waiting = (0..self.keys.len()).filter(|&i| missing(i) && !released(i)).count();
        let empty: Vec<usize> = (0..self.keys.len()).filter(|&i| self.assigned[i].is_none()).collect();
        for (pass, slots) in [replacing, empty].into_iter().enumerate() {
            for i in slots {
                // empty jobs leave a free dongle for each job still waiting for its own
                if free.is_empty() || (pass == 1 && free.len() <= waiting) {
                    break;
                }
                let new = free.remove(0);
                changes.push((i, self.assigned[i].clone(), new.clone()));
                self.assigned[i] = Some(new);
                self.missing_since.remove(&i);
            }
        }
        changes.sort_by_key(|c| c.0);
        self.spare = free;
        if !changes.is_empty() {
            if let Err(e) = self.save() {
                log::warn!("could not save {}: {e}", self.path.display());
            }
        }
        changes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use railspotting_common::FakeClock;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rs-store-assign-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn ch(i: usize, old: Option<&str>, new: &str) -> Change {
        (i, old.map(String::from), new.to_string())
    }

    #[test]
    fn auto_mode_detection() {
        assert!(auto_mode(&json!({"devices": []})));
        assert!(auto_mode(&json!({"devices": [{"serial": "auto", "role": "vhf"}, {"serial": "auto", "role": "eot"}]})));
        assert!(auto_mode(&json!({"devices": [{"serial": "00000001", "role": "vhf"}]}))); // old placeholders
        assert!(!auto_mode(&json!({"devices": [{"serial": "30001111", "role": "vhf"}]})));
        assert!(!auto_mode(&json!({"devices": [{"serial": 30001111, "role": "vhf"}]})));
        assert!(!auto_mode(&json!({"devices": [], "auto_assign": false})));
    }

    #[test]
    fn role_configs_defaults_and_overrides() {
        let slots = role_configs(&json!({"devices": []}));
        // EOT first, then VHF, then HOT
        assert_eq!(slots.iter().map(|s| s["role"].as_str().unwrap()).collect::<Vec<_>>(), vec!["eot", "vhf", "hot"]);
        assert_eq!(slots[2]["name"], "UHF SDR (HOT)");
        assert!(slots.iter().all(|s| s["serial"] == ""));
        assert_eq!(slots[1]["gain"], 20.7);
        assert_eq!(slots[0]["sample_rate"], 2_400_000.0);
        assert!(slots[1]["antenna"].as_str().unwrap().contains("longer"));
        assert!(slots[0]["antenna"].as_str().unwrap().contains("shorter"));
        let custom = role_configs(&json!({"devices": [{"serial": "auto", "role": "vhf", "gain": 15.0}]}));
        assert_eq!(custom[0]["gain"], 15.0);
        assert_eq!(custom[0]["name"], "VHF SDR (voice)");
        assert_eq!(slot_keys(&["vhf", "eot", "eot"]), vec!["vhf", "eot", "eot-2"]);
        // the key order sdr_roles.json has
        let keys: Vec<&String> = slots[0].as_object().unwrap().keys().collect();
        assert_eq!(keys, vec!["center_mhz", "name", "gain", "sample_rate", "antenna", "role", "serial"]);
        let other = role_configs(&json!({"devices": [{"role": "ais"}]}));
        assert_eq!(other[0]["name"], "AIS SDR");
        assert_eq!(other[0]["antenna"], UHF_ANTENNA);
    }

    #[test]
    fn assigns_in_order_and_is_stable() {
        let d = tmp("order");
        let clock = FakeClock::new(1_790_865_400.0);
        let mut a = RoleAssigner::new(&["vhf", "eot"], &d, clock.clock());
        let changes = a.update(&["30002222", "30001111"]);
        assert_eq!(changes, vec![ch(0, None, "30001111"), ch(1, None, "30002222")]);
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(d.join(ROLES_FILE)).unwrap()).unwrap();
        assert_eq!(saved, json!({"vhf": "30001111", "eot": "30002222"}));
        assert_eq!(std::fs::read_to_string(d.join(ROLES_FILE)).unwrap(),
                   "{\n  \"vhf\": \"30001111\",\n  \"eot\": \"30002222\"\n}\n");
        assert!(a.update(&["30001111", "30002222"]).is_empty()); // enumeration order doesn't matter
        let mut b = RoleAssigner::new(&["vhf", "eot"], &d, clock.clock()); // after a restart
        assert!(b.update(&["30002222", "30001111"]).is_empty());
        assert_eq!(b.get("eot"), Some("30002222"));
    }

    #[test]
    fn late_dongle_and_single_dongle() {
        let d = tmp("late");
        let clock = FakeClock::new(1_790_865_400.0);
        let mut a = RoleAssigner::new(&["vhf", "eot"], &d, clock.clock());
        assert_eq!(a.update(&["11111111"]), vec![ch(0, None, "11111111")]); // one dongle: voice first
        assert_eq!(a.update(&["11111111", "22222222"]), vec![ch(1, None, "22222222")]);
    }

    #[test]
    fn replug_keeps_role_replacement_takes_it_after_60_s() {
        let d = tmp("replug");
        let clock = FakeClock::new(1_790_865_400.0);
        let mut a = RoleAssigner::new(&["vhf", "eot"], &d, clock.clock());
        a.update(&["11111111", "22222222"]);
        assert!(a.update(&["22222222"]).is_empty()); // vhf dongle unplugged
        clock.advance(30.0);
        assert!(a.update(&["22222222", "33333333"]).is_empty()); // not yet: maybe a replug
        assert_eq!(a.spare, vec!["33333333"]);
        assert!(a.update(&["11111111", "22222222", "33333333"]).is_empty()); // it came back in time
        a.update(&["22222222", "33333333"]);
        clock.advance(61.0);
        assert_eq!(a.update(&["22222222", "33333333"]), vec![ch(0, Some("11111111"), "33333333")]);
        assert!(a.spare.is_empty());
    }

    #[test]
    fn duplicate_serials_are_not_used() {
        let d = tmp("dup");
        let clock = FakeClock::new(1_790_865_400.0);
        let mut a = RoleAssigner::new(&["vhf", "eot"], &d, clock.clock());
        assert_eq!(a.update(&["00000001", "00000001", "30001111"]), vec![ch(0, None, "30001111")]);
        assert_eq!(a.duplicates, vec!["00000001"]);
        assert_eq!(a.get("eot"), None);
        a.update(&["", "30001111"]);
        assert_eq!(a.duplicates, vec!["(none)"]);
    }

    fn choose(a: &mut RoleAssigner, pairs: &[(&str, &str)]) {
        a.chosen = pairs.iter().map(|(s, r)| (s.to_string(), r.to_string())).collect();
    }

    #[test]
    fn the_headend_choice_wins_and_the_rest_stay_automatic() {
        let d = tmp("chosen");
        let clock = FakeClock::new(1_790_865_400.0);
        let mut a = RoleAssigner::new(&["eot", "vhf"], &d, clock.clock());
        // automatic first: the first dongle takes EOT
        assert_eq!(a.update(&["30001111"]), vec![ch(0, None, "30001111")]);
        // the owner makes it the voice dongle: it moves, and EOT has none
        choose(&mut a, &[("30001111", "vhf")]);
        assert_eq!(a.update(&["30001111"]), vec![ch(0, Some("30001111"), ""), ch(1, None, "30001111")]);
        assert_eq!(a.as_json(), json!({"eot": null, "vhf": "30001111"}));
        // a second, unnamed dongle fills the free role by itself
        assert_eq!(a.update(&["30001111", "30002222"]), vec![ch(0, None, "30002222")]);
        // nothing changes on the next look
        assert!(a.update(&["30001111", "30002222"]).is_empty());
    }

    #[test]
    fn a_dongle_the_headend_turns_off_takes_no_role() {
        let d = tmp("off");
        let clock = FakeClock::new(1_790_865_400.0);
        let mut a = RoleAssigner::new(&["eot", "vhf"], &d, clock.clock());
        a.update(&["30001111", "30002222"]);
        choose(&mut a, &[("30002222", "off")]);
        assert_eq!(a.update(&["30001111", "30002222"]), vec![ch(1, Some("30002222"), "")]);
        assert!(a.spare.is_empty(), "an off dongle is not a spare");
        // a choice for a dongle that is not plugged in changes nothing
        choose(&mut a, &[("30009999", "eot")]);
        assert_eq!(a.update(&["30001111", "30002222"]), vec![ch(1, None, "30002222")]);
    }

    #[test]
    fn a_missing_dongle_does_not_hold_a_role_the_headend_gave_away() {
        // the case that started this: EOT kept waiting for a dongle that is gone while the new one
        // went to VHF; the owner names the new one for EOT
        let d = tmp("gone");
        let clock = FakeClock::new(1_790_865_400.0);
        std::fs::write(d.join(ROLES_FILE), r#"{"eot": "30001111", "vhf": "30002222"}"#).unwrap();
        let mut a = RoleAssigner::new(&["eot", "vhf"], &d, clock.clock());
        choose(&mut a, &[("30002222", "eot")]);
        assert_eq!(a.update(&["30002222"]), vec![ch(0, Some("30001111"), "30002222"), ch(1, Some("30002222"), "")]);
        assert_eq!(a.role_of(0), "eot");
    }

    #[test]
    fn second_slots_of_a_role() {
        let d = tmp("second");
        let clock = FakeClock::new(1_790_865_400.0);
        let mut a = RoleAssigner::new(&["eot", "eot"], &d, clock.clock());
        assert_eq!((a.role_of(0), a.role_of(1)), ("eot", "eot"));
        choose(&mut a, &[("30001111", "eot"), ("30002222", "eot")]);
        assert_eq!(a.update(&["30002222", "30001111"]).len(), 2);
        assert_eq!(a.as_json(), json!({"eot": "30001111", "eot-2": "30002222"}));
    }

    #[test]
    fn a_replacement_dongle_takes_the_job_of_the_one_thrown_away() {
        // EOT's dongle is gone; a new one is plugged in. It goes to EOT, not to the empty VHF.
        let d = tmp("replace");
        let clock = FakeClock::new(1_790_865_400.0);
        std::fs::write(d.join(ROLES_FILE), r#"{"eot": "30001111", "vhf": null}"#).unwrap();
        let mut a = RoleAssigner::new(&["eot", "vhf"], &d, clock.clock());
        // at first EOT may only be being replugged: the new dongle waits as a spare
        assert!(a.update(&["30002222"]).is_empty());
        assert_eq!(a.spare, vec!["30002222".to_string()]);
        clock.advance(RELEASE_AFTER_S);
        assert_eq!(a.update(&["30002222"]), vec![ch(0, Some("30001111"), "30002222")]);
        assert_eq!(a.as_json(), json!({"eot": "30002222", "vhf": null}));
        // the old one coming back is a spare, and fills VHF
        assert_eq!(a.update(&["30002222", "30001111"]), vec![ch(1, None, "30001111")]);
    }

    #[test]
    fn a_second_new_dongle_fills_the_empty_job_at_once() {
        let d = tmp("replace2");
        let clock = FakeClock::new(1_790_865_400.0);
        std::fs::write(d.join(ROLES_FILE), r#"{"eot": "30001111", "vhf": null}"#).unwrap();
        let mut a = RoleAssigner::new(&["eot", "vhf"], &d, clock.clock());
        // two new dongles: one is kept back for EOT, the other takes VHF now
        assert_eq!(a.update(&["30002222", "30003333"]), vec![ch(1, None, "30002222")]);
        clock.advance(RELEASE_AFTER_S);
        assert_eq!(a.update(&["30002222", "30003333"]), vec![ch(0, Some("30001111"), "30003333")]);
    }
}
