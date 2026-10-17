//! Scanner monitor thread: connection handling, GLG/PWR polling and scanner events.

use crate::scanner::{cable_serial_from_port, system_finder, system_opener, Finder, Opener, Scanner, ScannerError};
use crate::util::{median, round_even, round_int, truthy};
use crate::{real_sleep, Sleep, Timing};
use railspotting_common::{plan_channel, system_clock, Clock};
use serde_json::{json, Value};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

pub const POLL_S: f64 = 0.03;
pub const IDLE_PWR_S: f64 = 1.0;
pub const RETRY_S: f64 = 3.0;
pub const TRIGGER_POLLS: u32 = 2;
pub const CALIBRATE_S: f64 = 1.5;

pub type Emit = Arc<dyn Fn(Value) + Send + Sync>;
pub type GetConfig = Arc<dyn Fn() -> Value + Send + Sync>;
/// Setup after a connect (crash recovery, focus resume). An error or panic is logged with its
/// details, and the scanner stays connected.
pub type OnConnect = Arc<dyn Fn(&ScannerMonitor) -> Result<(), ScannerError> + Send + Sync>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// The hooks the receiver (or a test) gives the monitor. Defaults: the real cable, real time.
#[derive(Clone)]
pub struct Hooks {
    pub emit: Emit,
    pub get_config: GetConfig,
    pub opener: Opener,
    pub finder: Finder,
    pub clock: Clock,
    pub sleep: Sleep,
    pub on_connect: Option<OnConnect>,
}

impl Default for Hooks {
    fn default() -> Self {
        Hooks {
            emit: Arc::new(|_| {}),
            get_config: Arc::new(|| json!({"channels": []})),
            opener: system_opener(),
            finder: system_finder(),
            clock: system_clock(),
            sleep: real_sleep(),
            on_connect: None,
        }
    }
}

/// The scanner event being received.
#[derive(Debug, Clone)]
struct OpenEvent {
    event_id: String,
    start_ts: f64,
    end_ts: Option<f64>,
    freq: f64,
    ch: Value,
    kind: Value,
    label: Value,
    proximity: Value,
    rssi: Vec<i64>,
    last: f64,
}

#[derive(Default)]
struct State {
    connected: bool,
    port: Option<String>,
    model: Option<String>,
    firmware: Option<String>,
    busy_reason: Option<String>,
    volume: Option<i64>,
    squelch: Option<i64>,
    freq: Option<f64>,
    rssi: Option<i64>,
    event: Option<OpenEvent>,
    trigger_floor: Option<f64>,
    trigger_margin: f64,
    trigger_count: u32,
    last_pwr: f64,
    last_error: Option<String>,
    polls: u64,
}

/// Owns the scanner. Polling, checks (through `borrow`) and focus changes take turns on it.
pub struct ScannerMonitor {
    pub port_setting: String,
    pub baud: u32,
    hooks: Mutex<Hooks>,
    /// The scanner itself; held for a poll or a whole check.
    op: Mutex<Option<Scanner>>,
    state: Mutex<State>,
    /// The FocusManager's `active` focus (None when there is no focus or no manager).
    pub(crate) focus: Mutex<Option<Value>>,
    stop: Mutex<bool>,
    stop_cv: Condvar,
}

impl ScannerMonitor {
    pub fn new(port_setting: &str, baud: u32, hooks: Hooks) -> Self {
        ScannerMonitor {
            port_setting: if port_setting.is_empty() { "auto".into() } else { port_setting.into() },
            baud,
            hooks: Mutex::new(hooks),
            op: Mutex::new(None),
            state: Mutex::new(State { trigger_margin: 15.0, ..State::default() }),
            focus: Mutex::new(None),
            stop: Mutex::new(false),
            stop_cv: Condvar::new(),
        }
    }

    pub fn hooks(&self) -> Hooks {
        lock(&self.hooks).clone()
    }

    /// Swap hooks after construction (tests point an agent's monitor at a fake scanner).
    pub fn set_hooks(&self, f: impl FnOnce(&mut Hooks)) {
        f(&mut lock(&self.hooks));
    }

    pub fn timing(&self) -> Timing {
        let h = self.hooks();
        Timing { clock: h.clock, sleep: h.sleep }
    }

    fn now(&self) -> f64 {
        // only the clock, not every hook: this runs on each poll
        let clock = lock(&self.hooks).clock.clone();
        clock()
    }

    fn emit(&self, ev: Value) {
        let emit = self.hooks().emit;
        emit(ev);
    }

    // lifecycle -------------------------------------------------------------------------

    pub fn start(self: &Arc<Self>) -> std::thread::JoinHandle<()> {
        let me = self.clone();
        std::thread::Builder::new()
            .name("scanner-monitor".into())
            .spawn(move || me.run())
            .expect("spawn scanner-monitor thread")
    }

    pub fn stop(&self) {
        *lock(&self.stop) = true;
        self.stop_cv.notify_all();
    }

    fn stopped(&self) -> bool {
        *lock(&self.stop)
    }

    /// Wait up to `s` seconds; true when stop was asked for.
    fn wait_stop(&self, s: f64) -> bool {
        let g = lock(&self.stop);
        let (g, _) = self
            .stop_cv
            .wait_timeout_while(g, Duration::from_secs_f64(s), |stop| !*stop)
            .unwrap_or_else(|p| p.into_inner());
        *g
    }

    fn run(&self) {
        while !self.stopped() {
            if !self.connected() {
                if !self.try_connect() {
                    self.wait_stop(RETRY_S);
                }
                continue;
            }
            if let Err(e) = self.poll_once() {
                self.disconnect(e.msg());
                continue;
            }
            self.wait_stop(POLL_S);
        }
    }

    pub fn try_connect(&self) -> bool {
        let h = self.hooks();
        let port = if self.port_setting != "auto" { Some(self.port_setting.clone()) } else { None };
        let attempt = (|| -> Result<(Scanner, String, i64, i64), ScannerError> {
            let port = port.or_else(|| (h.finder)()).filter(|p| !p.is_empty())
                .ok_or_else(|| ScannerError::Io("no CP2104 cable found".into()))?;
            let mut scanner = Scanner::open(&port, self.baud, &h.opener, h.sleep.clone())?;
            if scanner.model.as_deref() != Some("SR30C") {
                log::warn!("scanner on {port} reports model {}; treating it like an SR30C",
                           scanner.model.as_deref().unwrap_or("None"));
            }
            let volume = scanner.volume()?;
            let squelch = scanner.squelch()?;
            Ok((scanner, port, volume, squelch))
        })();
        let (model, firmware, port) = match attempt {
            Ok((scanner, port, volume, squelch)) => {
                let mut op = lock(&self.op);
                let mut st = lock(&self.state);
                let mf = (scanner.model.clone(), scanner.firmware.clone());
                *op = Some(scanner);
                st.port = Some(port.clone());
                st.connected = true;
                st.model = mf.0.clone();
                st.firmware = mf.1.clone();
                st.volume = Some(volume);
                st.squelch = Some(squelch);
                st.last_error = None;
                (mf.0, mf.1, port)
            }
            Err(e) => {
                let msg = e.msg().to_string();
                let mut st = lock(&self.state);
                if st.last_error.as_deref() != Some(msg.as_str()) {
                    log::info!("scanner not available: {msg}");
                }
                st.last_error = Some(msg);
                return false;
            }
        };
        log::info!("scanner connected: {} {} on {port}", model.as_deref().unwrap_or("None"),
                   firmware.as_deref().unwrap_or("None"));
        if let Some(cb) = h.on_connect {
            // a panic in the receiver's setup must not take the monitor thread down
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cb(self))) {
                Ok(Ok(())) => {}
                Ok(Err(e)) => log::error!("scanner setup after connect failed: {e}"),
                Err(_) => log::error!("scanner setup after connect failed"),
            }
        }
        true
    }

    pub fn disconnect(&self, reason: &str) {
        let mut op = lock(&self.op);
        self.disconnect_locked(&mut op, reason);
    }

    /// Let go of the port on purpose (stopping or restarting), so it can be opened again.
    pub fn close(&self) {
        let mut op = lock(&self.op);
        if self.connected() {
            log::info!("scanner port closed");
        }
        self.release_locked(&mut op);
    }

    fn disconnect_locked(&self, op: &mut Option<Scanner>, reason: &str) {
        if self.connected() {
            log::warn!("scanner lost: {reason}");
        }
        self.release_locked(op);
    }

    fn release_locked(&self, op: &mut Option<Scanner>) {
        self.close_event(None);
        if let Some(mut s) = op.take() {
            s.close();
        }
        let mut st = lock(&self.state);
        st.connected = false;
        st.freq = None;
        st.rssi = None;
    }

    // borrowing the radio -----------------------------------------------------------------

    /// Hold the radio for a check. Polling pauses and the state reads 'busy'.
    ///
    /// Any scanner or port error inside drops the connection; the monitor reconnects.
    pub fn borrow<T>(&self, reason: &str, f: impl FnOnce(&mut Scanner) -> Result<T, ScannerError>) -> Result<T, ScannerError> {
        let mut op = lock(&self.op);
        if !self.connected() || op.is_none() {
            return Err(ScannerError::Io("no scanner connected".into()));
        }
        self.close_event(None);
        lock(&self.state).busy_reason = Some(reason.to_string());
        let r = match op.as_mut() {
            Some(s) => f(s),
            None => Err(ScannerError::Io("no scanner connected".into())),
        };
        if let Err(ScannerError::Io(msg)) = &r {
            lock(&self.state).busy_reason = None;
            self.disconnect_locked(&mut op, msg);
        }
        lock(&self.state).busy_reason = None;
        r
    }

    /// Run `f` on the scanner without marking it busy (tests, diagnostics).
    pub fn with_scanner<T>(&self, f: impl FnOnce(&mut Scanner) -> T) -> Option<T> {
        lock(&self.op).as_mut().map(f)
    }

    pub fn connected(&self) -> bool {
        lock(&self.state).connected
    }

    pub fn idle(&self) -> bool {
        let st = lock(&self.state);
        st.connected && st.busy_reason.is_none()
    }

    pub fn busy_reason(&self) -> Option<String> {
        lock(&self.state).busy_reason.clone()
    }

    pub fn port(&self) -> Option<String> {
        lock(&self.state).port.clone()
    }

    pub fn model(&self) -> Option<String> {
        lock(&self.state).model.clone()
    }

    pub fn firmware(&self) -> Option<String> {
        lock(&self.state).firmware.clone()
    }

    pub fn volume(&self) -> Option<i64> {
        lock(&self.state).volume
    }

    pub fn set_volume(&self, level: i64) {
        lock(&self.state).volume = Some(level);
    }

    pub fn squelch(&self) -> Option<i64> {
        lock(&self.state).squelch
    }

    pub fn set_squelch(&self, level: i64) {
        lock(&self.state).squelch = Some(level);
    }

    pub fn freq(&self) -> Option<f64> {
        lock(&self.state).freq
    }

    pub fn rssi(&self) -> Option<i64> {
        lock(&self.state).rssi
    }

    pub fn polls(&self) -> u64 {
        lock(&self.state).polls
    }

    pub fn last_error(&self) -> Option<String> {
        lock(&self.state).last_error.clone()
    }

    pub fn trigger_floor(&self) -> Option<f64> {
        lock(&self.state).trigger_floor
    }

    pub fn set_trigger_floor(&self, v: Option<f64>) {
        lock(&self.state).trigger_floor = v;
    }

    pub fn trigger_margin(&self) -> f64 {
        lock(&self.state).trigger_margin
    }

    pub fn set_trigger_margin(&self, v: f64) {
        lock(&self.state).trigger_margin = v;
    }

    pub fn trigger_count(&self) -> u32 {
        lock(&self.state).trigger_count
    }

    pub fn set_trigger_count(&self, n: u32) {
        lock(&self.state).trigger_count = n;
    }

    /// The focus a FocusManager has active, if any.
    pub fn focus_active(&self) -> Option<Value> {
        lock(&self.focus).clone()
    }

    // polling ------------------------------------------------------------------------------

    fn trigger_mode(&self) -> bool {
        lock(&self.focus).as_ref().is_some_and(|f| truthy(f.get("rssi_trigger")))
    }

    /// Median PWR over a short window, for the focus RSSI trigger.
    pub fn calibrate(&self, seconds: f64) -> Result<Option<f64>, ScannerError> {
        if let Some(f) = self.trigger_floor() {
            return Ok(Some(f));
        }
        let t = self.timing();
        let mut readings = Vec::new();
        let end = t.now() + seconds;
        {
            let mut op = lock(&self.op);
            while t.now() < end {
                let Some(s) = op.as_mut() else { break };
                readings.push(s.pwr()?.0 as f64);
                t.sleep(POLL_S);
            }
        }
        if let Some(m) = median(&readings) {
            lock(&self.state).trigger_floor = Some(m);
            log::info!("scanner trigger floor {:.0}", m);
        }
        Ok(self.trigger_floor())
    }

    pub fn poll_once(&self) -> Result<(), ScannerError> {
        let mut op = lock(&self.op);
        if lock(&self.state).busy_reason.is_some() {
            return Ok(());
        }
        let Some(s) = op.as_mut() else { return Ok(()) };
        let g = s.glg()?;
        let now = self.now();
        let trigger = self.trigger_mode();
        let need_pwr = {
            let mut st = lock(&self.state);
            st.polls += 1;
            let active = g.sql || st.event.is_some() || trigger;
            active || now - st.last_pwr >= IDLE_PWR_S
        };
        let mut rssi = None;
        if need_pwr {
            let (r, _pfreq) = s.pwr()?;
            let mut st = lock(&self.state);
            st.last_pwr = now;
            st.rssi = Some(r);
            rssi = Some(r);
        }
        let mut freq = g.freq;
        if trigger && freq.is_none() {
            freq = self.focus_active().and_then(|f| f.get("freq").and_then(Value::as_f64));
        }
        let receiving = {
            let mut st = lock(&self.state);
            st.freq = freq;
            let mut receiving = g.sql && freq.is_some();
            if trigger {
                if let (Some(r), Some(floor)) = (rssi, st.trigger_floor) {
                    if r as f64 >= floor + st.trigger_margin {
                        st.trigger_count += 1;
                    } else {
                        st.trigger_count = 0;
                    }
                    receiving = receiving || st.trigger_count >= TRIGGER_POLLS;
                }
            }
            receiving
        };
        self.track(receiving, freq, rssi, now);
        drop(op);
        Ok(())
    }

    fn track(&self, receiving: bool, freq: Option<f64>, rssi: Option<i64>, now: f64) {
        let has_event = {
            let st = lock(&self.state);
            st.event.as_ref().map(|ev| !receiving || freq != Some(ev.freq))
        };
        let mut have = has_event.is_some();
        if has_event == Some(true) {
            self.close_event(Some(now));
            have = false;
        }
        let Some(f) = freq.filter(|_| receiving) else { return };
        if !have {
            let cfg = (self.hooks().get_config)();
            let ch = plan_channel(&cfg, f);
            let get = |k: &str| ch.and_then(|c| c.get(k)).cloned().unwrap_or(Value::Null);
            let ev = OpenEvent {
                event_id: uuid::Uuid::new_v4().to_string(),
                start_ts: now,
                end_ts: None,
                freq: f,
                ch: get("ch"),
                kind: match ch {
                    Some(c) => c.get("kind").cloned().unwrap_or_else(|| json!("voice")),
                    None => json!("unknown"),
                },
                label: get("label"),
                proximity: get("proximity"),
                rssi: rssi.into_iter().collect(),
                last: now,
            };
            let payload = payload(&ev, "open");
            lock(&self.state).event = Some(ev);
            self.emit(payload);
        } else {
            let mut st = lock(&self.state);
            if let Some(ev) = st.event.as_mut() {
                ev.last = now;
                if let Some(r) = rssi {
                    ev.rssi.push(r);
                }
            }
        }
    }

    fn close_event(&self, now: Option<f64>) {
        let payload = {
            let mut st = lock(&self.state);
            let Some(mut ev) = st.event.take() else { return };
            ev.end_ts = Some(now.unwrap_or(ev.last));
            st.trigger_count = 0;
            payload(&ev, "closed")
        };
        self.emit(payload);
    }

    // reporting --------------------------------------------------------------------------------

    pub fn state(&self) -> &'static str {
        let st = lock(&self.state);
        if !st.connected {
            return "scanning";
        }
        if st.busy_reason.is_some() {
            return "busy";
        }
        if st.event.is_some() {
            return "receiving";
        }
        drop(st);
        if self.focus_active().is_some() {
            return "parked";
        }
        "scanning"
    }

    pub fn heartbeat_info(&self) -> Value {
        let state = self.state();
        let st = lock(&self.state);
        if !st.connected {
            return json!({"connected": false, "model": null, "firmware": null, "cable_serial": null,
                          "port": null, "state": null, "busy_reason": null, "freq": null,
                          "rssi": null, "volume": null, "squelch": null});
        }
        json!({"connected": true, "model": st.model, "firmware": st.firmware,
               "cable_serial": null, "port": null,           // privacy rule v1.1
               "state": state, "busy_reason": st.busy_reason,
               "freq": if state == "receiving" || state == "parked" { json!(st.freq) } else { Value::Null },
               "rssi": st.rssi, "volume": st.volume, "squelch": st.squelch})
    }

    /// For `status` only. Includes the cable serial, which never leaves the machine.
    pub fn local_info(&self) -> Value {
        let mut info = self.heartbeat_info();
        let port = self.port();
        info["cable_serial"] = json!(cable_serial_from_port(port.as_deref()));
        info["port"] = json!(port);
        info
    }
}

fn payload(ev: &OpenEvent, state: &str) -> Value {
    let r: &[i64] = if ev.rssi.is_empty() { &[0] } else { &ev.rssi };
    let peak = r.iter().copied().max().unwrap_or(0);
    let avg = round_int(r.iter().sum::<i64>() as f64 / r.len() as f64);
    json!({"event_id": ev.event_id, "state": state, "start_ts": round_even(ev.start_ts, 2),
           "end_ts": if state == "closed" { json!(ev.end_ts.map(|t| round_even(t, 2))) } else { Value::Null },
           "freq": round_even(ev.freq, 4), "ch": ev.ch, "kind": ev.kind,
           "peak_rssi": peak, "avg_rssi": avg,
           "samples": ev.rssi.len(), "_source": "scanner",
           "_label": ev.label, "_proximity": ev.proximity})
}
