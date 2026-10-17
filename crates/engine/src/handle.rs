//! The receiver as something a program can drive: start it, read its status, hear about
//! changes, change settings, stop it. The CLI and the desktop app both use this.
//!
//! The engine owns the status page rather than the agent, so the page keeps its port while a
//! settings change restarts the agent behind it.

use crate::agent::{Agent, AgentOptions, EventHook, LINK_TRIGGER};
use crate::web::{public_status, WebHost, WebUI};
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use railspotting_common::{merge, write_atomic, Settings};
use serde_json::{json, Map, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::thread::JoinHandle;
use std::time::Duration;

/// How long `stop` spends sending queued events before it gives up (they stay queued).
pub const STOP_FLUSH_S: f64 = 5.0;
/// How long `stop` waits for the agent's threads.
pub const STOP_JOIN_S: f64 = 10.0;
/// How often the status is checked for changes worth telling subscribers.
pub const WATCH_S: f64 = 1.0;
/// A subscriber that falls this far behind misses messages rather than holding up the engine.
pub const SUBSCRIBER_QUEUE: usize = 256;

pub const MAX_LABEL: usize = 64;
pub const MAX_WATCH_CLIPS: usize = 20;
pub const MAX_DEVICES: usize = 4;
pub const MAX_GAIN_DB: f64 = 50.0;
pub const ROLES: [&str; 2] = ["vhf", "eot"];

/// What subscribers hear.
#[derive(Debug, Clone, PartialEq)]
pub enum EngineEvent {
    /// The status changed in a way worth showing (registration, link, dongles, scanner). The
    /// value is what `/api/status` returns.
    Status(Value),
    /// An event was queued for upload.
    Event(Value),
    /// The link code to show, or None once there is none (linked, or not linking).
    Link(Option<Value>),
    /// Something the user should see, such as a restart that failed.
    Error(String),
    Stopped,
}

#[derive(Clone, Default)]
pub struct EngineOptions {
    pub agent: AgentOptions,
    /// The per-run token for the settings endpoints. None turns them off.
    pub token: Option<String>,
}

/// A fresh random token for the settings endpoints.
pub fn new_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// The agent's copy of the settings: the engine serves the page, not the agent.
fn agent_settings(s: &Settings) -> Settings {
    let mut out = s.clone();
    if let Some(web) = out.raw.get_mut("web").and_then(Value::as_object_mut) {
        web.insert("enabled".into(), json!(false));
    }
    out
}

/// The settings the dashboard can change, as they are now.
pub fn editable_settings(s: &Settings) -> Value {
    json!({
        "label": s.get("label"),
        "transcribe_other_calls": s.bool("transcribe_other_calls", false),
        "watch_clips": s.get("watch_clips").as_array().cloned().unwrap_or_default(),
        "sdr": {"devices": s.devices()},
    })
}

/// Check a settings change from the dashboard and return it cleaned up. Only the keys the
/// dashboard offers may change; anything else is refused, so the page can't touch the server
/// URL, the state folder or the bind address.
pub fn validate_settings(patch: &Value) -> Result<Value, String> {
    let obj = patch.as_object().ok_or("settings must be a JSON object")?;
    if obj.is_empty() {
        return Err("no settings given".into());
    }
    let mut out = Map::new();
    for (k, v) in obj {
        let clean = match k.as_str() {
            "label" => match v {
                Value::Null => Value::Null,
                Value::String(t) => {
                    let t = t.trim();
                    if t.chars().count() > MAX_LABEL {
                        return Err(format!("label is longer than {MAX_LABEL} characters"));
                    }
                    if t.chars().any(char::is_control) {
                        return Err("label has control characters".into());
                    }
                    if t.is_empty() { Value::Null } else { json!(t) }
                }
                _ => return Err("label must be text or null".into()),
            },
            "transcribe_other_calls" => match v {
                Value::Bool(b) => json!(b),
                _ => return Err("transcribe_other_calls must be true or false".into()),
            },
            "watch_clips" => {
                let list = v.as_array().ok_or("watch_clips must be a list of frequencies in MHz")?;
                if list.len() > MAX_WATCH_CLIPS {
                    return Err(format!("watch_clips has more than {MAX_WATCH_CLIPS} channels"));
                }
                let mut freqs = Vec::new();
                for f in list {
                    match f.as_f64() {
                        Some(x) if (24.0..=1766.0).contains(&x) => freqs.push(json!(x)),
                        _ => return Err(format!("watch_clips: {f} is not a frequency an RTL-SDR can tune (MHz)")),
                    }
                }
                Value::Array(freqs)
            }
            "sdr" => {
                let sdr = v.as_object().ok_or("sdr must be an object")?;
                if let Some(other) = sdr.keys().find(|k| *k != "devices") {
                    return Err(format!("sdr.{other} can't be changed here"));
                }
                let devices = sdr.get("devices").ok_or("sdr needs devices")?;
                json!({"devices": validate_devices(devices)?})
            }
            other => return Err(format!("{other} can't be changed here")),
        };
        out.insert(k.clone(), clean);
    }
    Ok(Value::Object(out))
}

fn validate_devices(devices: &Value) -> Result<Value, String> {
    let list = devices.as_array().ok_or("sdr.devices must be a list")?;
    if list.len() > MAX_DEVICES {
        return Err(format!("sdr.devices has more than {MAX_DEVICES} dongles"));
    }
    let mut out = Vec::new();
    let mut serials = Vec::new();
    for d in list {
        let d = d.as_object().ok_or("each dongle must be an object")?;
        let mut c = Map::new();
        for (k, v) in d {
            match k.as_str() {
                "role" => match v.as_str() {
                    Some(r) if ROLES.contains(&r) => { c.insert("role".into(), json!(r)); }
                    _ => return Err(format!("role must be one of {}", ROLES.join(", "))),
                },
                "serial" => match v.as_str() {
                    Some(s) if !s.is_empty() && s.len() <= 32 && s.chars().all(|ch| ch.is_ascii_alphanumeric()) => {
                        if serials.contains(&s.to_string()) {
                            return Err(format!("serial {s} is given twice"));
                        }
                        serials.push(s.to_string());
                        c.insert("serial".into(), json!(s));
                    }
                    _ => return Err("serial must be letters and digits".into()),
                },
                "gain" => match v.as_f64() {
                    Some(g) if (0.0..=MAX_GAIN_DB).contains(&g) => { c.insert("gain".into(), json!(g)); }
                    _ => return Err(format!("gain must be a number of dB from 0 to {MAX_GAIN_DB}")),
                },
                other => return Err(format!("sdr.devices: {other} can't be changed here")),
            }
        }
        if !c.contains_key("role") {
            return Err("each dongle needs a role".into());
        }
        out.push(Value::Object(c));
    }
    Ok(Value::Array(out))
}

/// Whether the headend answered the most recent request: None before any request. A network
/// error or a 5xx counts as unreachable; a 4xx means it answered.
pub fn headend_reachable(st: &Value) -> Option<bool> {
    let api = st.get("api")?.as_object()?;
    let ts = |r: &Value| r.get("ts").and_then(Value::as_f64).unwrap_or(f64::NEG_INFINITY);
    let last = api.values().max_by(|a, b| ts(a).total_cmp(&ts(b)))?;
    Some(last.get("status").and_then(Value::as_u64).is_some_and(|s| s < 500))
}

/// The parts of the status that are worth a `Status` message when they change. Counters and
/// signal levels change all the time and are left out.
fn status_summary(st: &Value) -> Value {
    let sdrs: Vec<Value> = st.get("sdrs").and_then(Value::as_array).into_iter().flatten()
        .map(|s| json!([s.get("role"), s.get("serial"), s.get("connected"), s.get("streaming")])).collect();
    let sc = st.get("scanner");
    json!({
        "registration": st.get("registration"), "device_id": st.get("device_id"),
        "link": st.get("link").and_then(|l| l.get("user_code")),
        "sdrs": sdrs,
        "scanner": [sc.and_then(|s| s.get("connected")), sc.and_then(|s| s.get("model"))],
        "clock_synced": st.get("clock_synced"),
        "scanner_role": st.get("scanner_role"),
        "headend": headend_reachable(st),
    })
}

struct Inner {
    settings: RwLock<Settings>,
    opts: AgentOptions,
    token: Option<String>,
    agent: RwLock<Option<Arc<Agent>>>,
    subs: Mutex<Vec<Sender<EngineEvent>>>,
    /// One restart or stop at a time.
    lifecycle: Mutex<()>,
    stopping: AtomicBool,
}

impl Inner {
    fn current(&self) -> Option<Arc<Agent>> {
        self.agent.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn send(&self, ev: EngineEvent) {
        lock(&self.subs).retain(|tx| !matches!(tx.try_send(ev.clone()), Err(TrySendError::Disconnected(_))));
    }

    fn start_agent(self: &Arc<Self>, settings: &Settings) -> Result<Arc<Agent>, String> {
        let agent = Agent::new(agent_settings(settings), self.opts.clone())?;
        let me = Arc::downgrade(self);
        let hook: EventHook = Arc::new(move |ev: &Value| {
            if let Some(inner) = me.upgrade() {
                inner.send(EngineEvent::Event(ev.clone()));
            }
        });
        agent.set_event_hook(Some(hook));
        agent.start();
        Ok(agent)
    }

    fn apply(self: &Arc<Self>, patch: Value) -> Result<Value, String> {
        let patch = validate_settings(&patch)?;
        let _guard = lock(&self.lifecycle);
        if self.stopping.load(Ordering::SeqCst) {
            return Err("the receiver is stopping".into());
        }
        let old = self.settings.read().unwrap_or_else(|p| p.into_inner()).clone();
        let path = old.path.clone().ok_or("these settings have no receiver.json to save to")?;
        let old_text = std::fs::read(&path).ok();
        let file: Value = match &old_text {
            Some(b) => serde_json::from_slice(b).map_err(|e| format!("{}: {e}", path.display()))?,
            None => json!({}),
        };
        let text = serde_json::to_string_pretty(&merge(&file, &patch)).map_err(|e| e.to_string())? + "\n";
        write_atomic(&path, text.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut new = old.clone();
        new.raw = merge(&old.raw, &patch);
        log::info!("settings changed ({}); restarting the receiver",
                   patch.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>().join(", ")).unwrap_or_default());
        if let Some(a) = self.agent.write().unwrap_or_else(|p| p.into_inner()).take() {
            a.shutdown(STOP_FLUSH_S, STOP_JOIN_S);
        }
        match self.start_agent(&new) {
            Ok(a) => {
                *self.agent.write().unwrap_or_else(|p| p.into_inner()) = Some(a);
                *self.settings.write().unwrap_or_else(|p| p.into_inner()) = new.clone();
                Ok(editable_settings(&new))
            }
            Err(e) => {
                // put the old settings back and carry on with them
                log::error!("receiver did not start with the new settings: {e}");
                let restored = match old_text {
                    Some(b) => write_atomic(&path, &b),
                    None => std::fs::remove_file(&path),
                };
                if let Err(re) = restored {
                    log::error!("{}: could not put the old settings back: {re}", path.display());
                }
                match self.start_agent(&old) {
                    Ok(a) => *self.agent.write().unwrap_or_else(|p| p.into_inner()) = Some(a),
                    Err(e2) => {
                        log::error!("receiver did not start again with the old settings: {e2}");
                        self.send(EngineEvent::Error(format!("the receiver stopped: {e2}")));
                    }
                }
                Err(format!("the receiver did not start with these settings: {e}"))
            }
        }
    }

    fn relink(&self) -> Result<Value, String> {
        let agent = self.current().ok_or("the receiver is not running")?;
        if let Some(id) = agent.device_id() {
            return Err(format!("this receiver is already linked as {id}"));
        }
        // the scheduler picks the trigger up within a few seconds, as for `railspotting-receiver link`
        std::fs::write(agent.state_dir.join(LINK_TRIGGER), b"").map_err(|e| e.to_string())?;
        agent.reg_wake.set();
        Ok(json!({"linking": true}))
    }
}

impl WebHost for Arc<Inner> {
    fn agent(&self) -> Option<Arc<Agent>> {
        self.current()
    }
    fn token(&self) -> Option<String> {
        self.token.clone()
    }
    fn settings(&self) -> Result<Value, String> {
        Ok(editable_settings(&self.settings.read().unwrap_or_else(|p| p.into_inner())))
    }
    fn apply_settings(&self, patch: Value) -> Result<Value, String> {
        self.apply(patch)
    }
    fn relink(&self) -> Result<Value, String> {
        Inner::relink(self)
    }
}

/// A running receiver.
pub struct Engine {
    inner: Arc<Inner>,
    web: Mutex<Option<WebUI>>,
    watcher: Mutex<Option<JoinHandle<()>>>,
}

impl Engine {
    /// Build and start everything. Returns once the threads are running.
    pub fn start(settings: Settings, opts: EngineOptions) -> Result<Engine, String> {
        let inner = Arc::new(Inner {
            settings: RwLock::new(settings.clone()),
            opts: opts.agent,
            token: opts.token,
            agent: RwLock::new(None),
            subs: Mutex::new(Vec::new()),
            lifecycle: Mutex::new(()),
            stopping: AtomicBool::new(false),
        });
        let agent = inner.start_agent(&settings)?;
        *inner.agent.write().unwrap_or_else(|p| p.into_inner()) = Some(agent);
        let web_cfg = settings.get("web");
        let web = if web_cfg.get("enabled").map(crate::util::truthy).unwrap_or(true) {
            let mut w = WebUI::with_host(Arc::new(inner.clone()), web_cfg.get("bind").and_then(Value::as_str).unwrap_or("0.0.0.0"),
                                         web_cfg.get("port").and_then(Value::as_u64).unwrap_or(8080) as u16);
            w.start();
            Some(w)
        } else {
            None
        };
        let watcher = {
            let inner = inner.clone();
            std::thread::Builder::new().name("engine-watch".into()).spawn(move || watch(&inner)).ok()
        };
        Ok(Engine { inner, web: Mutex::new(web), watcher: Mutex::new(watcher) })
    }

    /// What `/api/status` returns.
    pub fn status(&self) -> Value {
        self.inner.current().map(|a| public_status(&a.status())).unwrap_or_else(|| json!({"stopped": true}))
    }

    /// The agent running now. A settings change replaces it.
    pub fn agent(&self) -> Option<Arc<Agent>> {
        self.inner.current()
    }

    /// Status changes, events, the link code and errors, from now on.
    pub fn subscribe(&self) -> Receiver<EngineEvent> {
        let (tx, rx) = bounded(SUBSCRIBER_QUEUE);
        lock(&self.inner.subs).push(tx);
        rx
    }

    /// The port the status page listens on (0 picks a free one), when it runs.
    pub fn web_port(&self) -> Option<u16> {
        lock(&self.web).as_ref().filter(|w| w.running()).map(|w| w.port)
    }

    pub fn settings(&self) -> Settings {
        self.inner.settings.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Validate a change, save it to receiver.json and restart the receiver with it. When the
    /// receiver won't start, the old settings are put back.
    pub fn apply_settings(&self, patch: Value) -> Result<Value, String> {
        self.inner.apply(patch)
    }

    /// Start linking again (after a denial, or for a receiver that is not linked).
    pub fn relink(&self) -> Result<Value, String> {
        self.inner.relink()
    }

    /// Stop the radios, send what is queued (briefly), close the dongles and the scanner, and
    /// write the final status.
    pub fn stop(&self) {
        let _guard = lock(&self.inner.lifecycle);
        if self.inner.stopping.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(w) = lock(&self.web).as_mut() {
            w.stop();
        }
        if let Some(a) = self.inner.agent.write().unwrap_or_else(|p| p.into_inner()).take() {
            a.shutdown(STOP_FLUSH_S, STOP_JOIN_S);
        }
        if let Some(t) = lock(&self.watcher).take() {
            let _ = t.join();
        }
        self.inner.send(EngineEvent::Stopped);
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop();
    }
}

fn watch(inner: &Arc<Inner>) {
    let mut last: Option<Value> = None;
    let mut last_link: Option<Value> = None;
    while !inner.stopping.load(Ordering::SeqCst) {
        if let Some(a) = inner.current() {
            let st = public_status(&a.status());
            let summary = status_summary(&st);
            if last.as_ref() != Some(&summary) {
                inner.send(EngineEvent::Status(st.clone()));
                last = Some(summary);
            }
            let link = st.get("link").filter(|l| !l.is_null()).cloned();
            if link != last_link {
                inner.send(EngineEvent::Link(link.clone()));
                last_link = link;
            }
        }
        std::thread::sleep(Duration::from_secs_f64(WATCH_S));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_takes_only_dashboard_keys() {
        assert!(validate_settings(&json!({"server_url": "https://example.com"})).is_err());
        assert!(validate_settings(&json!({"web": {"bind": "0.0.0.0"}})).is_err());
        assert!(validate_settings(&json!({"sdr": {"margin_db": 2}})).is_err());
        assert!(validate_settings(&json!({})).is_err());
        assert!(validate_settings(&json!([1])).is_err());
        let ok = validate_settings(&json!({"label": "  Shed  ", "transcribe_other_calls": true,
                                           "watch_clips": [160.95], "sdr": {"devices": [
                                               {"role": "vhf", "serial": "30002222", "gain": 20.7},
                                               {"role": "eot", "serial": "30001111"}]}})).unwrap();
        assert_eq!(ok["label"], "Shed");
        assert_eq!(ok["sdr"]["devices"][1], json!({"role": "eot", "serial": "30001111"}));
        assert_eq!(validate_settings(&json!({"label": ""})).unwrap()["label"], Value::Null);
    }

    #[test]
    fn validation_refuses_bad_values() {
        for bad in [
            json!({"label": "x".repeat(65)}), json!({"label": "a\nb"}), json!({"label": 3}),
            json!({"transcribe_other_calls": "yes"}),
            json!({"watch_clips": [5.0]}), json!({"watch_clips": "160.95"}),
            json!({"sdr": {"devices": [{"role": "hf"}]}}),
            json!({"sdr": {"devices": [{"serial": "30001111"}]}}),
            json!({"sdr": {"devices": [{"role": "vhf", "serial": "30/1"}]}}),
            json!({"sdr": {"devices": [{"role": "vhf", "gain": 80}]}}),
            json!({"sdr": {"devices": [{"role": "vhf", "sample_rate": 1}]}}),
            json!({"sdr": {"devices": [{"role": "vhf", "serial": "30001111"}, {"role": "eot", "serial": "30001111"}]}}),
        ] {
            assert!(validate_settings(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn agent_never_serves_the_page_itself() {
        let s = Settings::from_value(&json!({"web": {"port": 9}}), "/r");
        let a = agent_settings(&s);
        assert_eq!(a.get("web")["enabled"], false);
        assert_eq!(a.get("web")["port"], 9);
        assert_eq!(s.get("web")["enabled"], true);
    }

    #[test]
    fn headend_reachable_from_the_last_request() {
        assert_eq!(headend_reachable(&json!({})), None);
        assert_eq!(headend_reachable(&json!({"api": {}})), None);
        let st = |a: Value, b: Value| json!({"api": {"/v1/heartbeat": {"ts": 10.0, "status": a},
                                                     "/v1/events": {"ts": 20.0, "status": b}}});
        assert_eq!(headend_reachable(&st(json!(200), json!(200))), Some(true));
        assert_eq!(headend_reachable(&st(json!(200), Value::Null)), Some(false)); // network error last
        assert_eq!(headend_reachable(&st(Value::Null, json!(400))), Some(true));
        assert_eq!(headend_reachable(&st(json!(200), json!(503))), Some(false));
    }

    #[test]
    fn summary_ignores_counters() {
        let a = json!({"registration": "approved", "outbox_depth": 1, "sdrs": [{"role": "vhf", "connected": true, "floor_db": 3}]});
        let mut b = a.clone();
        b["outbox_depth"] = json!(9);
        b["sdrs"][0]["floor_db"] = json!(7);
        assert_eq!(status_summary(&a), status_summary(&b));
        b["sdrs"][0]["connected"] = json!(false);
        assert_ne!(status_summary(&a), status_summary(&b));
    }
}
