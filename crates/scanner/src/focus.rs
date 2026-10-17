//! Scanner focus mode (parking on one frequency), scanner roles and the auto EOT watch.

use crate::monitor::{ScannerMonitor, CALIBRATE_S};
use crate::jsonfmt;
use crate::scanner::ScannerError;
use crate::util::{finally, fmt_g, scalar_eq, value_f64, round_even, value_str_opt, truthy};
use railspotting_common::{label_has, telemetry_channel, Clock};
use serde_json::{json, Map, Value};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

pub const FOCUS_CH: i64 = 151; // first channel of bank 4
pub const FOCUS_BANKS: &str = "1110111111"; // 0 = scanned: only bank 4
pub const ROLE_HOLD_S: f64 = 15.0;
pub const MANAGED_REASONS: [&str; 2] = ["role", "auto_eot"];
const ALL_BANKS_BUT_1_OFF: &str = "0111111111";

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn io(e: std::io::Error) -> ScannerError {
    ScannerError::Io(e.to_string())
}

// focus -------------------------------------------------------------------------------

/// Parks the SR30C on one frequency through a spare bank (channel 151, bank 4).
///
/// The active focus lives in the monitor (its `focus` field), so polling and
/// status see it.
pub struct FocusManager {
    monitor: Arc<ScannerMonitor>,
    pub path: PathBuf,
    clock: Clock,
    lock: Mutex<()>,
}

impl FocusManager {
    pub fn new(monitor: Arc<ScannerMonitor>, state_dir: &std::path::Path, clock: Clock) -> Self {
        *lock(&monitor.focus) = None; // this manager is now the monitor's, with nothing active
        FocusManager { monitor, path: state_dir.join("focus.json"), clock, lock: Mutex::new(()) }
    }

    pub fn monitor(&self) -> &Arc<ScannerMonitor> {
        &self.monitor
    }

    pub fn active(&self) -> Option<Value> {
        self.monitor.focus_active()
    }

    fn set_active(&self, v: Option<Value>) {
        *lock(&self.monitor.focus) = v;
    }

    fn save(&self) -> std::io::Result<()> {
        match self.active() {
            None => match std::fs::remove_file(&self.path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            },
            Some(a) => {
                if let Some(p) = self.path.parent() {
                    std::fs::create_dir_all(p)?;
                }
                std::fs::write(&self.path, jsonfmt::dumps_indent(&a, 2) + "\n")
            }
        }
    }

    pub fn start(&self, freq: f64, minutes: Option<f64>, reason: &str, rssi_trigger: bool, label: Option<&str>)
        -> Result<(), ScannerError> {
        {
            let _g = lock(&self.lock);
            self.monitor.borrow(&format!("focus {reason}"), |s| {
                s.prg()?;
                let r = (|| {
                    let mut original = match self.active() {
                        Some(a) => a.get("original_scg").cloned()
                            .ok_or_else(|| ScannerError::Value("'original_scg'".into()))?,
                        None => json!(s.scg()?),
                    };
                    if original == FOCUS_BANKS {
                        original = json!(ALL_BANKS_BUT_1_OFF);
                    }
                    let now = (self.clock)();
                    let end = minutes.filter(|m| *m != 0.0).map(|m| now + m * 60.0);
                    self.set_active(Some(json!({"freq": round_even(freq, 4), "reason": reason, "label": label,
                                                "rssi_trigger": rssi_trigger, "original_scg": original,
                                                "start_ts": now, "end_ts": end})));
                    self.save().map_err(io)?; // before touching memory, so a crash can undo it
                    s.cin_write(FOCUS_CH, freq, false, false)?;
                    s.set_scg(FOCUS_BANKS)
                })();
                finally(r, s.epg(true))
            })?;
        }
        self.monitor.set_trigger_count(0);
        if rssi_trigger {
            self.monitor.calibrate(CALIBRATE_S)?;
        }
        let for_min = match minutes.filter(|m| *m != 0.0) {
            Some(m) => format!(" for {} min", fmt_g(m)),
            None => String::new(),
        };
        log::info!("scanner focused on {:.4} MHz ({reason}){for_min}", freq);
        Ok(())
    }

    pub fn end(&self, why: &str) -> Result<(), ScannerError> {
        let _g = lock(&self.lock);
        self.end_locked(why)
    }

    fn end_locked(&self, why: &str) -> Result<(), ScannerError> {
        let Some(info) = self.active() else { return Ok(()) };
        self.monitor.borrow("focus end", |s| {
            s.prg()?;
            let r = (|| {
                s.cin_clear(FOCUS_CH)?;
                let orig = info.get("original_scg").and_then(Value::as_str).filter(|o| !o.is_empty())
                    .unwrap_or(ALL_BANKS_BUT_1_OFF).to_string();
                s.set_scg(&orig)
            })();
            finally(r, s.epg(true))
        })?;
        self.set_active(None);
        self.save().map_err(io)?;
        let freq = info.get("freq").and_then(Value::as_f64).unwrap_or(0.0);
        let why = if why.is_empty() { String::new() } else { format!(" ({why})") };
        log::info!("scanner focus on {:.4} MHz ended{why}", freq);
        Ok(())
    }

    /// After a restart: resume an unexpired focus, undo an expired one.
    pub fn recover(&self) -> Result<(), ScannerError> {
        let _g = lock(&self.lock);
        let saved: Value = match std::fs::read_to_string(&self.path).ok().and_then(|t| serde_json::from_str(&t).ok()) {
            Some(v @ Value::Object(_)) => v,
            _ => return Ok(()),
        };
        self.set_active(Some(saved.clone()));
        let end = saved.get("end_ts").and_then(Value::as_f64);
        if end.is_some_and(|e| e <= (self.clock)()) {
            self.end_locked("expired while stopped")
        } else {
            log::info!("resuming scanner focus on {:.4} MHz ({})", saved.get("freq").and_then(Value::as_f64).unwrap_or(0.0),
                       value_str_opt(saved.get("reason")));
            Ok(())
        }
    }

    pub fn tick(&self) -> Result<(), ScannerError> {
        let Some(a) = self.active() else { return Ok(()) };
        if let Some(end) = a.get("end_ts").and_then(Value::as_f64) {
            if (self.clock)() >= end {
                return self.end("time up");
            }
        }
        Ok(())
    }
}

// roles ---------------------------------------------------------------------------------

/// The scanner's job on Automatic: what no SDR covers, EOT first, then HOT, then the voice
/// channels (owner's decisions, October 2026: EOT first; HOT when an SDR has EOT).
pub fn desired_role(setting: &str, sdr_eot: bool, sdr_hot: bool) -> (String, String) {
    let r = |a: &str, b: &str| (a.to_string(), b.to_string());
    if !setting.is_empty() && setting != "auto" {
        return r(setting, "set in receiver.json");
    }
    if !sdr_eot {
        return r("eot", "no SDR is on EOT; EOT is the priority");
    }
    if !sdr_hot {
        return r("hot", "an SDR is on EOT");
    }
    r("scan", "SDRs are on EOT and HOT")
}

fn reason_of(a: &Value) -> Option<&str> {
    a.get("reason").and_then(Value::as_str)
}

/// Applies a scanner role once the desired role has been stable for 15 s.
pub struct RoleManager {
    focus: Option<Arc<FocusManager>>,
    clock: Clock,
    pub hold_s: f64,
    pub role: Option<String>,
    pub candidate: Option<String>,
    pub since: f64,
    pub reason: String,
}

impl RoleManager {
    pub fn new(focus: Option<Arc<FocusManager>>, clock: Clock) -> Self {
        Self::with_hold(focus, clock, ROLE_HOLD_S)
    }

    pub fn with_hold(focus: Option<Arc<FocusManager>>, clock: Clock, hold_s: f64) -> Self {
        RoleManager { focus, clock, hold_s, role: None, candidate: None, since: 0.0, reason: String::new() }
    }

    /// The role applied on this tick, if any.
    pub fn tick(&mut self, desired: &str, reason: &str, cfg: &Value, survey_running: bool) -> Result<Option<String>, ScannerError> {
        let now = (self.clock)();
        if self.candidate.as_deref() != Some(desired) {
            self.candidate = Some(desired.to_string());
            self.since = now;
            self.reason = reason.to_string();
        }
        if self.role.as_deref() == Some(desired) && self.in_place(desired, cfg) {
            return Ok(None);
        }
        if now - self.since < self.hold_s {
            return Ok(None);
        }
        let Some(focus) = self.focus.clone().filter(|f| f.monitor().connected()) else {
            self.role = Some(desired.to_string());
            return Ok(None);
        };
        let active = focus.active();
        if survey_running || !focus.monitor().idle()
            || active.as_ref().is_some_and(|a| !reason_of(a).is_some_and(|r| MANAGED_REASONS.contains(&r))) {
            return Ok(None); // remember it; apply when the scanner is free
        }
        self.apply(desired, cfg)?;
        Ok(Some(desired.to_string()))
    }

    fn in_place(&self, role: &str, cfg: &Value) -> bool {
        let active = self.focus.as_ref().and_then(|f| f.active());
        if role == "hot" || role == "eot" {
            let ch = telemetry_channel(cfg, role);
            return match (active, ch) {
                (Some(a), Some(c)) => {
                    let af = a.get("freq").and_then(Value::as_f64).unwrap_or(f64::NAN);
                    value_f64(c.get("freq")).is_ok_and(|cf| (af - cf).abs() < 1e-4)
                }
                _ => false,
            };
        }
        !active.is_some_and(|a| reason_of(&a) == Some("role"))
    }

    pub fn apply(&mut self, role: &str, cfg: &Value) -> Result<(), ScannerError> {
        log::info!("scanner role {role}: {}", self.reason);
        let Some(focus) = self.focus.clone() else {
            self.role = Some(role.to_string());
            return Ok(());
        };
        if role == "hot" || role == "eot" {
            match telemetry_channel(cfg, role) {
                None => log::warn!("scanner role {role} but the plan has no {} channel", role.to_uppercase()),
                Some(ch) => {
                    let f = value_f64(ch.get("freq")).map_err(ScannerError::Value)?;
                    let label = ch.get("label").and_then(Value::as_str);
                    focus.start(f, None, "role", true, label)?;
                }
            }
        } else if focus.active().is_some_and(|a| reason_of(&a) == Some("role")) {
            focus.end("role is now scan")?;
        }
        self.role = Some(role.to_string());
        Ok(())
    }
}

// auto EOT watch ------------------------------------------------------------------------

/// In the scan role with no SDR on EOT, watch EOT for a while after a nearby train.
pub struct AutoEotWatch {
    pub cfg: Value,
    focus: Option<Arc<FocusManager>>,
    clock: Clock,
    /// The last 20 watches; the newest is the current one once any has started.
    runs: Arc<Mutex<VecDeque<Value>>>,
    has_current: bool,
}

pub const MAX_RUNS: usize = 20;

fn num(v: Option<&Value>) -> f64 {
    v.and_then(Value::as_f64).unwrap_or(0.0)
}

impl AutoEotWatch {
    pub fn new(settings: Option<&Value>, focus: Option<Arc<FocusManager>>, clock: Clock) -> Self {
        let mut cfg: Map<String, Value> = json!({"enabled": true, "minutes": 30, "eot_rssi_margin": 15,
                                                 "hot_min_rssi": 0, "road_trigger": true, "road_min_rssi": 420})
            .as_object().cloned().unwrap_or_default();
        if let Some(Value::Object(s)) = settings {
            for (k, v) in s {
                cfg.insert(k.clone(), v.clone());
            }
        }
        AutoEotWatch { cfg: Value::Object(cfg), focus, clock, runs: Arc::new(Mutex::new(VecDeque::new())), has_current: false }
    }

    pub fn runs(&self) -> Vec<Value> {
        lock(&self.runs).iter().cloned().collect()
    }

    pub fn current(&self) -> Option<Value> {
        if self.has_current { lock(&self.runs).back().cloned() } else { None }
    }

    /// The trigger description if this event should start a watch.
    pub fn trigger_for(&self, event: &Value, cfg: &Value) -> Option<String> {
        if !truthy(self.cfg.get("enabled")) {
            return None;
        }
        let ev_freq = value_f64(event.get("freq")).ok()?;
        let ch = cfg.get("channels").and_then(Value::as_array)?.iter().find(|c| {
            scalar_eq(c.get("ch"), event.get("ch")) && value_f64(c.get("freq")).is_ok_and(|f| (f - ev_freq).abs() < 0.002)
        })?;
        let state = event.get("state").and_then(Value::as_str);
        let kind = ch.get("kind").and_then(Value::as_str);
        let peak = num(event.get("peak_rssi"));
        if state == Some("open") && kind == Some("telemetry") && label_has(ch, "HOT")
            && peak >= num(self.cfg.get("hot_min_rssi")) {
            return Some("HOT".into());
        }
        if state == Some("closed") && truthy(self.cfg.get("road_trigger")) && kind == Some("voice")
            && truthy(ch.get("proximity")) && peak >= num(self.cfg.get("road_min_rssi")) {
            return Some(format!("road channel {}", value_str_opt(ch.get("label"))));
        }
        None
    }

    pub fn consider(&mut self, event: &Value, cfg: &Value, role: Option<&str>, sdr_covers_eot: bool) -> bool {
        if role != Some("scan") || sdr_covers_eot {
            return false;
        }
        let Some(focus) = self.focus.clone() else { return false };
        if focus.active().is_some() {
            let label = event.get("_label").and_then(Value::as_str).unwrap_or("");
            if self.has_current && event.get("kind").and_then(Value::as_str) == Some("telemetry")
                && event.get("state").and_then(Value::as_str) == Some("closed")
                && label_has(&json!({"label": label}), "EOT") {
                let mut runs = lock(&self.runs);
                if let Some(cur) = runs.back_mut() {
                    cur["eot_bursts"] = json!(cur["eot_bursts"].as_i64().unwrap_or(0) + 1);
                    let peak = event.get("peak_rssi").filter(|v| truthy(Some(v))).cloned().unwrap_or(json!(0));
                    if num(Some(&peak)) > num(cur.get("peak_rssi")) {
                        cur["peak_rssi"] = peak;
                    }
                }
            }
            return false;
        }
        let Some(trig) = self.trigger_for(event, cfg) else { return false };
        let Some(eotch) = telemetry_channel(cfg, "EOT").cloned() else { return false };
        let Ok(eot_freq) = value_f64(eotch.get("freq")) else { return false };
        let minutes = num(self.cfg.get("minutes"));
        let run = json!({"trigger": trig, "rssi": event.get("peak_rssi"), "start_ts": (self.clock)(),
                         "eot_bursts": 0, "peak_rssi": 0});
        {
            let mut runs = lock(&self.runs);
            runs.push_back(run);
            while runs.len() > MAX_RUNS {
                runs.pop_front();
            }
        }
        self.has_current = true;

        let margin = num(self.cfg.get("eot_rssi_margin"));
        let rssi = value_str_opt(event.get("peak_rssi"));
        let label = eotch.get("label").and_then(Value::as_str).map(String::from);
        let go = move || {
            focus.monitor().set_trigger_margin(margin);
            match focus.start(eot_freq, Some(minutes), "auto_eot", true, label.as_deref()) {
                Ok(()) => log::info!("{trig} heard (RSSI {rssi}): watching EOT only for {} min", fmt_g(minutes)),
                Err(e) => log::warn!("could not start the EOT watch: {e}"),
            }
        };
        if let Err(e) = std::thread::Builder::new().name("auto-eot".into()).spawn(go) {
            log::warn!("could not start the EOT watch: {e}");
        }
        true
    }
}
