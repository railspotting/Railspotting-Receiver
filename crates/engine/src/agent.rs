//! The receiver agent: threads, scheduler, linking, uploads, heartbeats and status.

use crate::device::{rtl_factory, DeviceFactory, SdrHandle, SimDev};
use crate::rtl::DriverError;
use crate::transcribe::Transcriber;
use crate::util::{json_dumps_indent, repr_text, value_str, value_str_opt, truthy};
use crate::web::WebUI;
use crossbeam_channel::{unbounded, Receiver, Sender};
use railspotting_common::{load_cached_config, plan_channel, save_cached_config, Clock, Settings, AGENT_VERSION};
use railspotting_detect::detectors::headend_detector;
use railspotting_dsp::sdr::{SdrSource, TranscriptDone};
use railspotting_dsp::sim::{SimDevice, TrafficHandle, World};
use railspotting_proto::keys::{self, PrivateKey};
use railspotting_proto::link::{self, CODE_LOG_S, SLOW_DOWN_S, START_MIN_INTERVAL_S};
use railspotting_proto::{ApiClient, ApiError, Backoff};
use railspotting_scanner::checks;
use railspotting_scanner::fake::FakeSR30C;
use railspotting_scanner::monitor::Hooks;
use railspotting_scanner::scanner::{system_finder, system_opener, BANDS_MHZ};
use railspotting_scanner::{desired_role, AutoEotWatch, CommandHost, CommandRunner, FocusManager, RoleManager,
                           ScannerError, ScannerMonitor, Timing};
use railspotting_store::assign::{auto_mode, role_configs, RoleAssigner};
use railspotting_store::history::History;
use railspotting_store::identity::{load_or_create_identity, save_identity, valid_device_id, SYS_NET};
use railspotting_store::outbox::Outbox;
use railspotting_store::Signal;
use serde_json::{json, Map, Value};
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, Weak};
use std::thread::JoinHandle;

pub const SCHED_S: f64 = 5.0;
pub const REG_POLL_S: f64 = 30.0;
pub const FIRST_RADIO_CHECK_S: f64 = 20.0;
pub const RADIO_CHECK_RETRY_S: f64 = 30.0;
pub const REF_WEAK_DB: f64 = 10.0;
pub const REF_WEAK_FOR_S: f64 = 60.0;
pub const REF_REPEAT_S: f64 = 300.0;
pub const SIGNAL_LOG_S: f64 = 300.0;
pub const HISTORY_SIGNAL_S: f64 = 60.0;
pub const TT_SECONDS: f64 = 20.0;
pub const TT_WAIT_S: f64 = 25.0;
pub const TT_TRIGGER: &str = "run_transcription_test";
pub const TT_RESULT: &str = "transcription_test.json";
pub const LINK_TRIGGER: &str = "start_link";
pub const LINK_IDLE_S: f64 = 5.0;
pub const INSTALLED_CONFIG: &str = "/opt/railspotting-receiver/receiver.json";
pub const SIM_SERIALS: [&str; 2] = ["SIM00001", "SIM00002"];
pub const SDR_SCAN_S: f64 = 10.0; // dongle list through librtlsdr, when sysfs can't be read
pub const USB_CHECK_S: f64 = 2.0; // USB presence through sysfs (touches no device)

/// The dongles librtlsdr lists ({index, serial, vendor, product}).
pub type Lister = Arc<dyn Fn() -> Result<Vec<Value>, DriverError> + Send + Sync>;
/// The dongles on USB ({serial, port}), None when sysfs can't tell.
pub type UsbLister = Arc<dyn Fn() -> Option<Vec<Value>> + Send + Sync>;

/// What the agent needs from a transcriber; test doubles implement only `submit`.
pub trait AgentTranscriber: Send + Sync {
    fn submit(&self, audio: Vec<f32>, done: TranscriptDone, tag: &str, event: Option<Value>);
    /// Calls other than detectors are transcribed (the heartbeat's `transcription` feature).
    fn other_calls(&self) -> bool {
        true
    }
    fn start(&self) {}
    fn stop(&self) {}
    fn last_reject(&self) -> Option<Value> {
        None
    }
    /// The detector templates, None when it has no templates folder.
    fn templates(&self) -> Option<Vec<Map<String, Value>>> {
        None
    }
    /// The headend's detector list from /config.
    fn set_known_detectors(&self, _d: Option<Vec<Value>>) {}
    /// The headend's nearby_detectors from /config (contract v1.6).
    fn set_nearby_detectors(&self, _d: Option<Vec<Value>>) {}
    /// Whether the headend accepts `method: "words"` (contract v1.9).
    fn set_words_method(&self, _on: bool) {}
    /// Detector voice profiles, for the status page.
    fn profiles(&self) -> Result<Value, String> {
        Err("profiles are not offered here".into())
    }
    /// The channel plan and voice profiles from /config.
    fn set_profile_config(&self, _cfg: &Value) {}
    /// Read a recording with a profile (by id, or a profile given in full).
    fn test_profile(&self, _audio: &[f32], _profile: &Value) -> Result<Value, String> {
        Err("profiles are not offered here".into())
    }
}

impl AgentTranscriber for Transcriber {
    fn submit(&self, audio: Vec<f32>, done: TranscriptDone, tag: &str, event: Option<Value>) {
        Transcriber::submit(self, audio, done, tag, event)
    }
    fn other_calls(&self) -> bool {
        Transcriber::other_calls(self) && self.speech_available()
    }
    fn start(&self) {
        Transcriber::start(self)
    }
    fn stop(&self) {
        Transcriber::stop(self)
    }
    fn last_reject(&self) -> Option<Value> {
        Transcriber::last_reject(self)
    }
    fn templates(&self) -> Option<Vec<Map<String, Value>>> {
        self.template_list()
    }
    fn set_known_detectors(&self, d: Option<Vec<Value>>) {
        Transcriber::set_known_detectors(self, d)
    }
    fn set_nearby_detectors(&self, d: Option<Vec<Value>>) {
        Transcriber::set_nearby_detectors(self, d)
    }
    fn set_words_method(&self, on: bool) {
        Transcriber::set_words_method(self, on)
    }
    fn profiles(&self) -> Result<Value, String> {
        Ok(self.profiles_json())
    }

    fn set_profile_config(&self, cfg: &Value) {
        Transcriber::set_profile_config(self, cfg)
    }
    fn test_profile(&self, audio: &[f32], profile: &Value) -> Result<Value, String> {
        let p = match profile {
            Value::String(id) => self.find_profile(id).ok_or_else(|| format!("no profile {id}"))?,
            v => serde_json::from_value(v.clone()).map_err(|e| format!("not a profile: {e}"))?,
        };
        Transcriber::test_profile(self, audio, &p)
    }
}

/// The SDR side's view of the agent's transcriber.
struct DspTr(Arc<dyn AgentTranscriber>);

impl railspotting_dsp::sdr::Transcriber for DspTr {
    fn submit(&self, audio: Vec<f32>, done: TranscriptDone, tag: &str, event: Option<Value>) {
        self.0.submit(audio, done, tag, event)
    }
    fn other_calls(&self) -> bool {
        self.0.other_calls()
    }
}

/// How the agent is built: the options a caller may set.
#[derive(Default, Clone)]
pub struct AgentOptions {
    pub simulate: bool,
    pub clock: Option<Clock>,
    pub world: Option<Arc<World>>,
    pub fake_scanner: Option<Arc<FakeSR30C>>,
    pub transcriber: Option<Arc<dyn AgentTranscriber>>,
    pub lister: Option<Lister>,
    pub usb_lister: Option<UsbLister>,
    pub device_factory: Option<DeviceFactory>,
}

/// The link being shown to the owner.
#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    pub link_id: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: Value,
    pub expires_at: f64,
    pub interval: f64,
}

struct Reg {
    status: String,
    link: Option<Link>,
    last_link_start: Option<f64>,
    next_code_log: f64,
    backoff: Backoff,
}

#[derive(Default)]
struct Sched {
    next_sdr_scan: f64,
    next_usb_check: f64,
    sdr_scan_error: Option<String>,
    next_radio_check: f64,
    ref_weak_since: Option<f64>,
    ref_fail_sent: Option<f64>,
    ref_failing: bool,
    signal_samples: Vec<(f64, f64)>,
    next_signal_log: f64,
    /// Null until synced; a plan without a version is Null too, so it never counts as synced.
    plan_synced_version: Value,
    next_history_signal: f64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// `timedatectl show -p NTPSynchronized`: true/false, None when unknown (common inside containers).
pub fn timedatectl_synced() -> Option<bool> {
    let mut child = std::process::Command::new("timedatectl")
        .args(["show", "-p", "NTPSynchronized", "--value"])
        .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null()).spawn().ok()?;
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(st)) => {
                let mut out = String::new();
                use std::io::Read;
                child.stdout.take()?.read_to_string(&mut out).ok()?;
                let v = out.trim().to_lowercase();
                if !st.success() || (v != "yes" && v != "no") {
                    return None;
                }
                return Some(v == "yes");
            }
            Ok(None) if start.elapsed().as_secs_f64() < 5.0 => std::thread::sleep(std::time::Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

fn obj(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

pub struct Agent {
    me: Weak<Agent>,
    pub settings: Settings,
    pub simulate: bool,
    pub clock: Clock,
    pub started: f64,
    pub state_dir: PathBuf,
    pub stop_event: Signal,

    identity: Mutex<Value>,
    pub key: PrivateKey,
    reg: Mutex<Reg>,
    /// A device ID is stored: v1 calls may run.
    pub approved: Signal,
    pub reg_wake: Signal,
    /// Send a heartbeat now: the dongles changed.
    pub hb_wake: Signal,
    /// The dongle serials last seen plugged in (automatic roles).
    present_serials: Mutex<Vec<String>>,
    /// The scanner's ID while one is connected, to heartbeat at once when that changes (v1.12).
    last_scanner_id: Mutex<Option<String>>,
    /// Where the scanner's job came from: "automatic", "website" or "receiver.json".
    scanner_job_from: Mutex<String>,
    id_lock: Mutex<()>,

    config: RwLock<Value>,
    etag: Mutex<Option<String>>,
    pub outbox: Outbox,
    pub history: History,
    pub api: ApiClient,
    pub ntp: Option<bool>,
    recent_events: Mutex<VecDeque<Value>>,
    last_event_ts: Mutex<Value>,
    batch_limit: Mutex<Option<i64>>,
    /// Accepted since start: events, transcripts, telemetry.
    uploaded: Mutex<[u64; 3]>,

    pub world: Option<Arc<World>>,
    transcriber: RwLock<Option<Arc<dyn AgentTranscriber>>>,
    lister: Lister,
    usb_lister: UsbLister,
    /// With no dongle serials in receiver.json, each role takes a connected dongle by itself.
    pub auto_sdr: bool,
    sdrs: RwLock<Vec<Arc<SdrHandle>>>,
    factory: Option<DeviceFactory>,
    assigner: Option<Mutex<RoleAssigner>>,
    sched: Mutex<Sched>,
    tt_pending: Mutex<Option<f64>>,
    last_heartbeat: Mutex<Value>,

    pub fake_scanner: Option<Arc<FakeSR30C>>,
    pub monitor: Arc<ScannerMonitor>,
    pub focus: Arc<FocusManager>,
    roles: Mutex<RoleManager>,
    scanner_role: Mutex<Option<String>>,
    auto_eot: Mutex<AutoEotWatch>,
    commands: (Sender<Value>, Receiver<Value>),
    web: Mutex<Option<WebUI>>,
    traffic: Mutex<Option<TrafficHandle>>,
    threads: Mutex<Vec<JoinHandle<()>>>,
    /// Called with each event as it is queued, for the app (`handle::Engine::subscribe`).
    event_hook: RwLock<Option<EventHook>>,
}

pub type EventHook = Arc<dyn Fn(&Value) + Send + Sync>;

impl Agent {
    pub fn new(settings: Settings, opts: AgentOptions) -> Result<Arc<Agent>, String> {
        let clock = opts.clock.clone().unwrap_or_else(railspotting_common::system_clock);
        let state_dir = settings.state_dir();
        std::fs::create_dir_all(&state_dir).map_err(|e| format!("{}: {e}", state_dir.display()))?;
        let identity = load_or_create_identity(&state_dir, None, Path::new(SYS_NET))?;
        let device_id = identity.get("device_id").and_then(Value::as_str).map(String::from);
        let key = keys::load_or_create_key(&settings.credentials_path(), true)?;
        let reg_status = if device_id.is_some() { "checking" } else { "linking" };

        let mut config = load_cached_config(&state_dir);
        let etag = config.as_object_mut().and_then(|o| o.shift_remove("_etag")).and_then(|v| v.as_str().map(String::from));
        let outbox = Outbox::with(state_dir.join("outbox.db"), railspotting_store::outbox::DEFAULT_CAP, clock.clone())
            .map_err(|e| format!("outbox: {e:?}"))?;
        let history = History::with(state_dir.join("history.db"), railspotting_store::history::KEEP_DAYS, clock.clone())
            .map_err(|e| format!("history: {e:?}"))?;
        let api = ApiClient::new(settings.str("server_url").unwrap_or(""), device_id.clone(), key.clone(),
                                 settings.str("ca_file"), clock.clone(), 20.0).map_err(|e| e.to_string())?;
        let ntp = timedatectl_synced();

        // radios
        let sdr = settings.sdr().clone();
        let mut transcriber = opts.transcriber.clone();
        if transcriber.is_none() && settings.raw.get("transcription").is_none_or(truthy) {
            let keep = sdr.get("voice_clip_keep").and_then(Value::as_f64).unwrap_or(50.0) as usize;
            let other = truthy(settings.get("transcribe_other_calls"));
            let tr = Transcriber::new(&settings.models_dir(), Some(&state_dir.join("voice_clips")), keep, None,
                                      Some(&state_dir.join("detector_templates")), other);
            // settings "watch_clips": [MHz, ...] keeps every clip from those channels
            let watch: Vec<f64> = settings.get("watch_clips").as_array().map(|a| a.iter().filter_map(Value::as_f64).collect())
                .unwrap_or_default();
            if !watch.is_empty() {
                log::info!("keeping every clip from {} MHz in watch_clips",
                           watch.iter().map(|f| format!("{f:.4}")).collect::<Vec<_>>().join(", "));
            }
            tr.set_watch_freqs(watch);
            transcriber = Some(tr as Arc<dyn AgentTranscriber>);
        }
        let simulate = opts.simulate;
        let world = if simulate { Some(opts.world.clone().unwrap_or_else(|| Arc::new(World::default()))) } else { opts.world.clone() };
        let mut lister: Lister = opts.lister.clone().unwrap_or_else(|| Arc::new(crate::rtl::list_devices));
        // simulated or injected dongles aren't on USB, so sysfs says nothing about them
        let usb_lister: UsbLister = opts.usb_lister.clone().unwrap_or_else(|| {
            if !simulate && opts.lister.is_none() {
                Arc::new(|| railspotting_store::usb::plugged_in_here().map(|d| d.iter().map(|x| x.to_json()).collect()))
            } else {
                Arc::new(|| None)
            }
        });
        let mut factory = opts.device_factory.clone();
        if simulate {
            let (w, c) = (world.clone().unwrap_or_default(), clock.clone());
            if factory.is_none() {
                factory = Some(Arc::new(move |serial: &str| {
                    Ok(Arc::new(SimDev(SimDevice::new(serial, w.clone(), c.clone(), true))) as Arc<dyn crate::device::Device>)
                }));
            }
            if opts.lister.is_none() {
                lister = Arc::new(|| Ok(SIM_SERIALS.iter().enumerate().map(|(i, s)| {
                    json!({"index": i, "serial": s, "vendor": "Nooelec", "product": "NESDR SMArt v5"})
                }).collect()));
            }
        }
        if factory.is_none() {
            factory = Some(rtl_factory());
        }
        let auto_sdr = auto_mode(&sdr);
        let devices = if auto_sdr { role_configs(&sdr) } else { settings.devices() };

        let fake_scanner = if simulate {
            let rssi: Arc<dyn railspotting_scanner::fake::RssiSource> = Arc::new(WorldRssi(world.clone().unwrap_or_default()));
            Some(opts.fake_scanner.clone().unwrap_or_else(|| Arc::new(FakeSR30C::new(rssi, clock.clone()))))
        } else {
            opts.fake_scanner.clone()
        };
        let started = clock();
        let web_cfg = settings.get("web").clone();

        let agent = Arc::new_cyclic(|me: &Weak<Agent>| {
            let emit_me = me.clone();
            let emit: railspotting_dsp::sdr::Emit = Arc::new(move |ev: Map<String, Value>| {
                if let Some(a) = emit_me.upgrade() {
                    a.emit(ev);
                }
            });
            let dsp_tr: Option<Arc<dyn railspotting_dsp::sdr::Transcriber>> =
                transcriber.clone().map(|t| Arc::new(DspTr(t)) as Arc<dyn railspotting_dsp::sdr::Transcriber>);
            let sdrs: Vec<Arc<SdrHandle>> = devices.iter().map(|d| {
                SdrHandle::new(SdrSource::new(d, &sdr, &state_dir, emit.clone(), dsp_tr.clone(), clock.clone()), factory.clone())
            }).collect();
            let roles: Vec<String> = sdrs.iter().map(|s| s.with(|x| x.role.clone())).collect();
            let assigner = if auto_sdr { Some(RoleAssigner::new(&roles, &state_dir, clock.clone())) } else { None };
            if let Some(a) = &assigner {
                for (s, serial) in sdrs.iter().zip(&a.assigned) {
                    s.with(|x| x.serial = serial.clone().unwrap_or_default());
                }
            }
            let (opener, finder) = match &fake_scanner {
                Some(f) if simulate => (f.opener(), Arc::new(|| Some("/dev/fake-sr30c".to_string())) as railspotting_scanner::scanner::Finder),
                _ => (system_opener(), system_finder()),
            };
            let (m1, m2, m3) = (me.clone(), me.clone(), me.clone());
            let hooks = Hooks {
                emit: Arc::new(move |ev: Value| {
                    if let Some(a) = m1.upgrade() {
                        // turned off on the website (v1.12): what it hears is not reported
                        if a.scanner_role().as_deref() != Some("off") {
                            a.emit(obj(ev));
                        }
                    }
                }),
                get_config: Arc::new(move || m2.upgrade().map(|a| a.config()).unwrap_or_else(|| json!({"channels": []}))),
                opener, finder,
                clock: clock.clone(),
                sleep: railspotting_scanner::real_sleep(),
                on_connect: Some(Arc::new(move |m: &ScannerMonitor| match m3.upgrade() {
                    Some(a) => a.scanner_connected(m),
                    None => Ok(()),
                })),
            };
            let port = settings.str("serial_port").unwrap_or("auto").to_string();
            let baud = settings.get("baud").as_u64().unwrap_or(57600) as u32;
            let monitor = Arc::new(ScannerMonitor::new(&port, baud, hooks));
            let focus = Arc::new(FocusManager::new(monitor.clone(), &state_dir, clock.clone()));
            let roles = RoleManager::new(Some(focus.clone()), clock.clone());
            let auto_eot = AutoEotWatch::new(Some(settings.get("auto_eot_focus")), Some(focus.clone()), clock.clone());
            let web = if web_cfg.get("enabled").map(truthy).unwrap_or(true) {
                Some(WebUI::new(me.clone(), web_cfg.get("bind").and_then(Value::as_str).unwrap_or("0.0.0.0"),
                                web_cfg.get("port").and_then(Value::as_u64).unwrap_or(8080) as u16))
            } else {
                None
            };
            Agent {
                me: me.clone(),
                simulate,
                clock: clock.clone(),
                started,
                state_dir: state_dir.clone(),
                stop_event: Signal::default(),
                identity: Mutex::new(identity),
                key,
                reg: Mutex::new(Reg { status: reg_status.into(), link: None, last_link_start: None, next_code_log: 0.0,
                                      backoff: Backoff::default() }),
                approved: Signal::default(),
                reg_wake: Signal::default(),
                hb_wake: Signal::default(),
                present_serials: Mutex::new(Vec::new()),
                last_scanner_id: Mutex::new(None),
                scanner_job_from: Mutex::new("automatic".into()),
                id_lock: Mutex::new(()),
                config: RwLock::new(config),
                etag: Mutex::new(etag),
                outbox, history, api, ntp,
                recent_events: Mutex::new(VecDeque::new()),
                last_event_ts: Mutex::new(Value::Null),
                batch_limit: Mutex::new(None),
                uploaded: Mutex::new([0; 3]),
                world: world.clone(),
                transcriber: RwLock::new(transcriber),
                lister, usb_lister,
                auto_sdr,
                sdrs: RwLock::new(sdrs),
                factory,
                assigner: assigner.map(Mutex::new),
                sched: Mutex::new(Sched {
                    next_radio_check: started + FIRST_RADIO_CHECK_S,
                    next_signal_log: started + SIGNAL_LOG_S,
                    next_history_signal: started + HISTORY_SIGNAL_S,
                    ..Sched::default()
                }),
                tt_pending: Mutex::new(None),
                last_heartbeat: Mutex::new(Value::Null),
                fake_scanner,
                monitor, focus,
                roles: Mutex::new(roles),
                scanner_role: Mutex::new(None),
                auto_eot: Mutex::new(auto_eot),
                commands: unbounded(),
                web: Mutex::new(web),
                traffic: Mutex::new(None),
                threads: Mutex::new(vec![]),
                event_hook: RwLock::new(None),
                settings,
            }
        });
        Ok(agent)
    }

    fn arc(&self) -> Option<Arc<Agent>> {
        self.me.upgrade()
    }

    // accessors -------------------------------------------------------------------------

    pub fn config(&self) -> Value {
        self.config.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub fn set_config_value(&self, cfg: Value) {
        *self.config.write().unwrap_or_else(|p| p.into_inner()) = cfg;
    }

    pub fn device_id(&self) -> Option<String> {
        self.api.device_id()
    }

    pub fn identity(&self) -> Value {
        lock(&self.identity).clone()
    }

    pub fn reg_status(&self) -> String {
        lock(&self.reg).status.clone()
    }

    fn set_reg_status(&self, s: &str) {
        lock(&self.reg).status = s.into();
    }

    pub fn link(&self) -> Option<Link> {
        lock(&self.reg).link.clone()
    }

    /// Tests: change the link being shown (e.g. a short expiry).
    pub fn update_link(&self, f: impl FnOnce(&mut Option<Link>)) {
        f(&mut lock(&self.reg).link)
    }

    pub fn sdrs(&self) -> Vec<Arc<SdrHandle>> {
        self.sdrs.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub fn add_sdr(&self, h: Arc<SdrHandle>) {
        self.sdrs.write().unwrap_or_else(|p| p.into_inner()).push(h);
    }

    pub fn transcriber(&self) -> Option<Arc<dyn AgentTranscriber>> {
        self.transcriber.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub fn set_transcriber(&self, t: Option<Arc<dyn AgentTranscriber>>) {
        *self.transcriber.write().unwrap_or_else(|p| p.into_inner()) = t;
    }

    pub fn uploaded(&self) -> Value {
        let u = lock(&self.uploaded);
        json!({"events": u[0], "transcripts": u[1], "telemetry": u[2]})
    }

    pub fn batch_limit(&self) -> Option<i64> {
        *lock(&self.batch_limit)
    }

    pub fn scanner_role(&self) -> Option<String> {
        lock(&self.scanner_role).clone()
    }

    pub fn tt_pending(&self) -> Option<f64> {
        *lock(&self.tt_pending)
    }

    pub fn next_command(&self) -> Option<Value> {
        self.commands.1.try_recv().ok()
    }

    pub fn assigner_info(&self) -> Option<(Vec<String>, Vec<String>)> {
        self.assigner.as_ref().map(|a| {
            let a = lock(a);
            (a.duplicates.clone(), a.spare.clone())
        })
    }

    // events and reports --------------------------------------------------------------------

    pub fn emit(&self, event: Map<String, Value>) {
        let clean: Map<String, Value> = event.iter().filter(|(k, _)| !k.starts_with('_'))
            .map(|(k, v)| (k.clone(), v.clone())).collect();
        let clean_v = Value::Object(clean.clone());
        // the headend refuses telemetry with more corrected bits than it takes: send the burst
        // without it (it still shows here)
        let mut upload = clean.clone();
        if railspotting_dsp::eot::fit_for_headend(&mut upload, railspotting_dsp::eot::corrected_max(&self.config())) {
            log::info!("EOT packet with 2 corrected bits kept on this receiver; the headend does not take them yet");
        }
        if let Err(e) = self.outbox.add_event(&Value::Object(upload)) {
            log::error!("outbox write failed: {e:?}");
        }
        let hook = self.event_hook.read().unwrap_or_else(|p| p.into_inner()).clone();
        if let Some(h) = hook {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| h(&clean_v))).is_err() {
                log::error!("event hook failed");
            }
        }
        {
            let mut r = lock(&self.recent_events);
            r.push_back(clean_v.clone());
            while r.len() > 20 {
                r.pop_front();
            }
        }
        let cfg = self.config();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let ch = clean.get("freq").filter(|f| truthy(f)).and_then(Value::as_f64).and_then(|f| plan_channel(&cfg, f));
            let label = event.get("_label").filter(|v| truthy(v)).cloned()
                .unwrap_or_else(|| ch.and_then(|c| c.get("label")).cloned().unwrap_or(Value::Null));
            let mut h = clean.clone();
            h.insert("label".into(), label);
            h.insert("source".into(), event.get("_source").cloned().unwrap_or_else(|| json!("sdr")));
            self.history.add_event(&Value::Object(h)).map_err(|e| format!("{e:?}"))
        }));
        match r {
            Ok(Ok(())) => {}
            Ok(Err(e)) => log::error!("event history write failed: {e}"),
            Err(_) => log::error!("event history write failed"),
        }
        let ts = clean.get("end_ts").filter(|v| truthy(v)).or_else(|| clean.get("start_ts")).cloned().unwrap_or(Value::Null);
        *lock(&self.last_event_ts) = ts;
        let role = self.scanner_role();
        let covers = self.sdr_covers("eot");
        let ev = Value::Object(event);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            lock(&self.auto_eot).consider(&ev, &cfg, role.as_deref(), covers);
        }));
        if r.is_err() {
            log::error!("auto EOT watch failed");
        }
    }

    pub fn report(&self, rtype: &str, data: Value) {
        let rep = json!({"type": rtype, "ts": crate::util::round_even((self.clock)(), 1), "data": data});
        if let Err(e) = self.outbox.add_report(&rep) {
            log::error!("outbox write failed: {e:?}");
        }
    }

    pub fn sdr_covers(&self, what: &str) -> bool {
        let sdrs = self.sdrs();
        if what == "voice" {
            return sdrs.iter().any(|s| s.covers("voice", None));
        }
        let w = what.to_uppercase();
        sdrs.iter().any(|s| s.covers("telemetry", Some(&w)))
    }

    // config --------------------------------------------------------------------------------

    pub fn apply_config(&self, cfg: Value, etag: Option<String>) {
        let old = self.config().get("config_version").cloned();
        self.set_config_value(cfg.clone());
        *lock(&self.etag) = etag.clone();
        if let Err(e) = save_cached_config(&self.state_dir, &cfg, etag.as_deref()) {
            log::warn!("could not save the channel plan: {e}");
        }
        self.check_detectors(&cfg);
        // dongle jobs chosen on the website (v1.11)
        if self.assigner.is_some() {
            let present = lock(&self.present_serials).clone();
            self.apply_assignment(&present);
        }
        let chans = cfg.get("channels").and_then(Value::as_array).cloned().unwrap_or_default();
        for s in self.sdrs() {
            s.with(|x| x.set_plan(&chans, cfg.get("reference").filter(|r| !r.is_null())));
        }
        if old.as_ref() != cfg.get("config_version") {
            log::info!("channel plan version {} applied ({} channels)", value_str_opt(cfg.get("config_version")), chans.len());
        }
    }

    /// API contract v1.5 section 5: hand the headend's detector list to the transcriber, and
    /// warn about templates whose railroad and milepost it doesn't list.
    pub fn check_detectors(&self, cfg: &Value) -> Vec<String> {
        let Some(tr) = self.transcriber() else { return vec![] };
        let Some(templates) = tr.templates() else { return vec![] };
        let listed = cfg.get("detectors").and_then(Value::as_array).cloned();
        tr.set_known_detectors(listed.clone());
        tr.set_nearby_detectors(cfg.get("nearby_detectors").and_then(Value::as_array).cloned());
        tr.set_profile_config(cfg);
        // contract v1.9: the headend lists the detector methods it accepts
        tr.set_words_method(cfg.get("detector_methods").and_then(Value::as_array)
            .is_some_and(|m| m.iter().any(|x| x == "words")));
        let Some(listed) = listed.filter(|l| !l.is_empty()) else { return vec![] };
        let mut missing = vec![];
        for t in templates {
            let p = t.get("parsed").filter(|p| truthy(p)).cloned().unwrap_or_else(|| json!({}));
            let rr = p.get("railroad").filter(|v| truthy(v));
            let mp = p.get("milepost").filter(|v| !v.is_null());
            if let (Some(rr), Some(mp)) = (rr, mp) {
                if headend_detector(Some(&listed), &value_str(rr), mp.as_f64()).is_none() {
                    let name = value_str_opt(t.get("name"));
                    log::warn!("detector template {} ({} milepost {}) matches none of the headend's detectors; \
                                check its milepost", name, value_str(rr), value_str(mp));
                    missing.push(name);
                }
            }
        }
        missing
    }

    pub fn fetch_config(&self) -> Result<(), ApiError> {
        let etag = lock(&self.etag).clone();
        let (cfg, etag) = self.api.get_config(etag.as_deref())?;
        self.set_reg_status("approved");
        if let Some(cfg) = cfg {
            self.apply_config(cfg, etag);
        }
        Ok(())
    }

    // linking (API contract v1.4) -------------------------------------------------------------

    pub fn link_body(&self) -> Value {
        let info = self.monitor.heartbeat_info();
        let mut body = json!({
            "public_key": keys::public_b64(&self.key),
            "hostname": crate::util::head(&crate::util::hostname(), 253),
            "platform": crate::util::head(&crate::util::platform(), 200),
            "agent_version": AGENT_VERSION,
            "scanner": {"model": info.get("model").cloned().unwrap_or(Value::Null),
                        "firmware": info.get("firmware").cloned().unwrap_or(Value::Null),
                        "cable_serial": null, "port": null},
        });
        if let Some(mac) = self.identity().get("mac").filter(|m| truthy(m)) {
            body["mac"] = mac.clone();
        }
        body
    }

    pub fn show_link_code(&self, force: bool) {
        let now = (self.clock)();
        let mut reg = lock(&self.reg);
        let Some(lk) = reg.link.clone() else { return };
        if !force && now < reg.next_code_log {
            return;
        }
        reg.next_code_log = now + CODE_LOG_S;
        drop(reg);
        let left = (lk.expires_at - now).max(0.0);
        log::info!("LINK CODE {}  (open {}, sign in and enter the code to add this receiver; expires in {} min)",
                   link::format_user_code(&lk.user_code), lk.verification_uri, (left / 60.0).round_ties_even() as i64);
    }

    /// The owner approved the link: keep the assigned ID and run as a v1 receiver.
    pub fn adopt_device_id(&self, device_id: &Value) -> Result<(), ApiError> {
        if !valid_device_id(device_id) {
            let shown = match device_id {
                Value::String(s) => repr_text(s),
                v => value_str(v),
            };
            return Err(ApiError::new(None, "bad_device_id", &format!("headend assigned an invalid device ID {shown}"), None, None));
        }
        let id = value_str(device_id);
        {
            let _g = lock(&self.id_lock);
            self.api.set_device_id(Some(id.clone()));
            let mut ident = lock(&self.identity);
            if let Some(o) = ident.as_object_mut() {
                o.insert("device_id".into(), json!(id));
                o.insert("linked_at".into(), json!(crate::util::round_int((self.clock)())));
                o.insert("link_denied".into(), json!(false));
            }
            self.save_identity(&ident);
            let mut reg = lock(&self.reg);
            reg.link = None;
            reg.status = "approved".into();
        }
        log::info!("receiver linked as {id}");
        self.on_device_id();
        Ok(())
    }

    fn save_identity(&self, ident: &Value) {
        if let Err(e) = save_identity(&self.state_dir, ident) {
            log::error!("could not save identity.json: {e}");
        }
    }

    /// A device ID is stored: fetch the plan and let uploads and heartbeats run.
    pub fn on_device_id(&self) {
        if let Err(e) = self.fetch_config() {
            self.handle_api_error(&e);
            if self.device_id().is_some() {
                log::warn!("config fetch failed: {e}; using the cached plan");
            }
        }
        if self.device_id().is_some() {
            self.approved.set();
        }
    }

    /// 403 revoked (or the headend no longer knows the ID): link again with the same key.
    pub fn forget_device_id(&self, reason: &str) {
        {
            let _g = lock(&self.id_lock);
            let Some(old) = self.device_id() else { return };
            log::warn!("headend says {reason} for {old}; forgetting the device ID and linking again");
            self.approved.clear();
            self.api.set_device_id(None);
            let mut ident = lock(&self.identity);
            if let Some(o) = ident.as_object_mut() {
                o.insert("device_id".into(), Value::Null);
            }
            self.save_identity(&ident);
            let mut reg = lock(&self.reg);
            reg.link = None;
            reg.last_link_start = None;
            reg.status = "linking".into();
        }
        self.reg_wake.set();
    }

    fn start_link(&self, now: f64) -> Result<f64, ApiError> {
        lock(&self.reg).last_link_start = Some(now);
        let (status, body) = self.api.link_start(&self.link_body())?;
        lock(&self.reg).backoff.reset();
        if status == 200 && body.get("status").and_then(Value::as_str) == Some("approved") {
            self.adopt_device_id(body.get("device_id").unwrap_or(&Value::Null))?;
            return Ok(0.0);
        }
        let s = |k: &str| body.get(k).and_then(Value::as_str).map(String::from)
            .ok_or_else(|| ApiError::new(Some(status), "bad_reply", &format!("link reply without {k}"), None, None));
        let num = |k: &str, d: f64| body.get(k).filter(|v| !v.is_null()).map_or(Some(d), |v| v.as_f64()
            .or_else(|| v.as_str().and_then(|x| x.trim().parse().ok()))).unwrap_or(d);
        let lk = Link {
            link_id: s("link_id")?, user_code: s("user_code")?, verification_uri: s("verification_uri")?,
            verification_uri_complete: body.get("verification_uri_complete").cloned().unwrap_or(Value::Null),
            expires_at: now + num("expires_in", 900.0),
            interval: num("interval", 5.0),
        };
        let interval = lk.interval;
        {
            let mut reg = lock(&self.reg);
            reg.link = Some(lk);
            reg.status = "waiting".into();
        }
        self.show_link_code(true);
        Ok(interval)
    }

    fn poll_link(&self, now: f64) -> Result<f64, ApiError> {
        let Some(lk) = self.link() else { return Ok(0.0) };
        let body = match self.api.link_poll(&lk.link_id) {
            Ok(b) => b,
            Err(e) if e.status == Some(429) && e.error == "slow_down" => {
                let mut reg = lock(&self.reg);
                let interval = match reg.link.as_mut() {
                    Some(l) => {
                        l.interval += SLOW_DOWN_S;
                        l.interval
                    }
                    None => lk.interval + SLOW_DOWN_S,
                };
                drop(reg);
                log::info!("headend asked to poll more slowly; every {interval:.0} s now");
                return Ok(interval);
            }
            Err(e) if e.status == Some(404) => {
                log::warn!("headend does not know link {}; starting a new one", lk.link_id);
                lock(&self.reg).link = None;
                return Ok(0.0);
            }
            Err(e) => return Err(e),
        };
        lock(&self.reg).backoff.reset();
        match body.get("status").and_then(Value::as_str) {
            Some("pending") => {
                let interval = {
                    let mut reg = lock(&self.reg);
                    let Some(l) = reg.link.as_mut() else { return Ok(0.0) };
                    if let Some(e) = body.get("expires_in").and_then(Value::as_f64) {
                        l.expires_at = now + e;
                    }
                    if let Some(i) = body.get("interval").filter(|v| truthy(v)).and_then(Value::as_f64) {
                        l.interval = l.interval.max(i);
                    }
                    l.interval
                };
                self.show_link_code(false);
                Ok(interval)
            }
            Some("approved") => {
                self.adopt_device_id(body.get("device_id").unwrap_or(&Value::Null))?;
                Ok(0.0)
            }
            st => {
                lock(&self.reg).link = None;
                if st == Some("denied") {
                    let mut ident = lock(&self.identity);
                    if let Some(o) = ident.as_object_mut() {
                        o.insert("link_denied".into(), json!(true));
                    }
                    self.save_identity(&ident);
                    drop(ident);
                    self.set_reg_status("denied");
                    log::error!("the owner denied linking this receiver; run `railspotting-receiver link` to start again");
                    return Ok(LINK_IDLE_S);
                }
                log::info!("link code expired; starting a new link");
                Ok(0.0)
            }
        }
    }

    /// One linking action. Returns the seconds to wait before the next one.
    pub fn link_step(&self) -> f64 {
        if self.device_id().is_some() {
            return LINK_IDLE_S;
        }
        if truthy(self.identity().get("link_denied").unwrap_or(&Value::Null)) {
            self.set_reg_status("denied");
            return LINK_IDLE_S;
        }
        let now = (self.clock)();
        let r = (|| -> Result<f64, ApiError> {
            {
                let mut reg = lock(&self.reg);
                if reg.link.as_ref().is_some_and(|l| now >= l.expires_at) {
                    log::info!("link code expired; starting a new link");
                    reg.link = None;
                }
                if reg.link.is_none() {
                    if let Some(last) = reg.last_link_start {
                        let wait = last + START_MIN_INTERVAL_S - now;
                        if wait > 0.0 {
                            reg.status = "linking".into();
                            return Ok(wait);
                        }
                    }
                    drop(reg);
                    return self.start_link(now);
                }
            }
            self.poll_link(now)
        })();
        match r {
            Ok(w) => w,
            Err(e) => {
                self.handle_api_error(&e);
                let mut reg = lock(&self.reg);
                if reg.link.is_none() {
                    reg.status = "linking".into();
                }
                let d = reg.backoff.next_delay(e.retry_after);
                drop(reg);
                log::warn!("linking failed: {e}; retry in {d:.0} s");
                d
            }
        }
    }

    pub fn registration_loop(&self, until_approved_only: bool) {
        self.registration_loop_with(until_approved_only, &|| self.link_step())
    }

    /// The loop with its linking step swapped (tests).
    pub fn registration_loop_with(&self, until_approved_only: bool, step: &dyn Fn() -> f64) {
        while !self.stop_event.is_set() {
            if self.device_id().is_some() && !self.approved.is_set() {
                self.on_device_id();
            }
            let wait = if self.device_id().is_some() {
                if until_approved_only {
                    return;
                }
                LINK_IDLE_S
            } else {
                let w = step();
                if self.device_id().is_some() {
                    continue;
                }
                if until_approved_only && self.reg_status() == "denied" {
                    return;
                }
                w
            };
            self.reg_wake.wait(wait.max(0.0));
            self.reg_wake.clear();
        }
    }

    /// `railspotting-receiver link` asks the service to start linking again after a denial.
    pub fn link_trigger_tick(&self) -> Result<(), String> {
        let trigger = self.state_dir.join(LINK_TRIGGER);
        if !trigger.exists() {
            return Ok(());
        }
        let _ = std::fs::remove_file(&trigger);
        if let Some(id) = self.device_id() {
            log::info!("link requested, but this receiver is already linked as {id}");
            return Ok(());
        }
        log::info!("link requested by the owner; starting a new link");
        {
            let mut ident = lock(&self.identity);
            if let Some(o) = ident.as_object_mut() {
                o.insert("link_denied".into(), json!(false));
            }
            self.save_identity(&ident);
        }
        {
            let mut reg = lock(&self.reg);
            reg.link = None;
            reg.last_link_start = None;
            reg.status = "linking".into();
        }
        self.reg_wake.set();
        Ok(())
    }

    pub fn handle_api_error(&self, e: &ApiError) {
        if (e.status == Some(403) && ["pending", "rejected", "revoked"].contains(&e.error.as_str()))
            || (e.status == Some(401) && e.error == "unknown_device") {
            self.forget_device_id(&e.error);
        } else if e.is_clock_error() || e.error == "clock_skew" {
            let _ = self.api.probe_clock();
        }
    }

    // uploader ----------------------------------------------------------------------------------

    pub fn upload_once(&self) -> Result<bool, ApiError> {
        let cfg = self.config();
        let upload = cfg.get("upload").filter(|u| truthy(u)).cloned().unwrap_or_else(|| json!({}));
        let limit = self.batch_limit().unwrap_or_else(|| upload.get("max_batch").and_then(Value::as_f64).unwrap_or(200.0) as i64);
        let batch = self.outbox.next_events(limit).unwrap_or_else(|e| {
            log::error!("outbox read failed: {e:?}");
            vec![]
        });
        if !batch.is_empty() {
            let events: Vec<Value> = batch.iter().map(|(_, e)| e.clone()).collect();
            let resp = match self.api.post_events(&events) {
                Ok(r) => r,
                Err(e) if e.status == Some(400) || e.status == Some(413) => {
                    if batch.len() == 1 {
                        log::warn!("headend refused event {} ({}); dropped", value_str_opt(batch[0].1.get("event_id")), e.error);
                        let _ = self.outbox.delete(batch[0].0);
                        return Ok(true);
                    }
                    *lock(&self.batch_limit) = Some(((batch.len() / 2) as i64).max(1));
                    return Ok(true);
                }
                Err(e) => return Err(e),
            };
            *lock(&self.batch_limit) = None;
            let ids = |k: &str| -> Vec<String> {
                let mut seen = HashSet::new();
                resp.get(k).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str)
                    .filter(|s| seen.insert(s.to_string())).map(String::from).collect()
            };
            let (accepted, duplicates) = (ids("accepted"), ids("duplicates"));
            let acked: Vec<String> = accepted.iter().chain(&duplicates).cloned().collect();
            if let Err(e) = self.outbox.ack_events(&batch, &acked) {
                log::error!("outbox ack failed: {e:?}");
            }
            self.note_uploaded(&events, &accepted.into_iter().collect(), &duplicates.into_iter().collect());
        }
        let reports = self.outbox.next_reports(10).unwrap_or_default();
        for (rid, rep) in &reports {
            if let Err(e) = self.api.post_report(rep) {
                if e.status == Some(400) || e.status == Some(413) {
                    log::warn!("headend refused a {} report ({}); dropped", value_str_opt(rep.get("type")), e.error);
                } else {
                    return Err(e);
                }
            }
            let _ = self.outbox.delete(*rid);
        }
        Ok(!batch.is_empty() || !reports.is_empty())
    }

    /// Count what the headend took, and say so for every transcript.
    pub fn note_uploaded(&self, events: &[Value], accepted: &HashSet<String>, duplicates: &HashSet<String>) {
        for ev in events {
            let eid = ev.get("event_id").and_then(Value::as_str).unwrap_or("");
            let tr = ev.get("transcript").filter(|t| truthy(t));
            let text = || crate::util::head(tr.and_then(|t| t.get("text")).and_then(Value::as_str).unwrap_or(""), 80);
            if accepted.contains(eid) {
                let mut u = lock(&self.uploaded);
                u[0] += 1;
                if ev.get("telemetry").is_some_and(truthy) {
                    u[2] += 1;
                }
                if tr.is_some() {
                    u[1] += 1;
                    drop(u);
                    log::info!("headend accepted the transcript for {:.4} MHz ch {}: \"{}\"",
                               ev.get("freq").and_then(Value::as_f64).unwrap_or(0.0), value_str_opt(ev.get("ch")), text());
                }
            } else if duplicates.contains(eid) && tr.is_some() {
                log::warn!("headend called the closed event {} with a transcript a duplicate, so it may not have stored \"{}\"",
                           eid, text());
            }
        }
    }

    pub fn uploader_loop(&self) {
        let mut backoff = Backoff::default();
        while !self.stop_event.is_set() {
            if !self.approved.wait(1.0) {
                continue;
            }
            match self.upload_once() {
                Ok(sent) => {
                    backoff.reset();
                    if !sent {
                        let flush = self.config().get("upload").and_then(|u| u.get("flush_interval_s"))
                            .and_then(Value::as_f64).unwrap_or(2.0);
                        self.outbox.wakeup.wait(flush);
                        self.outbox.wakeup.clear();
                    }
                }
                Err(e) => {
                    self.handle_api_error(&e);
                    let d = backoff.next_delay(e.retry_after);
                    log::warn!("upload failed: {}; retry in {:.0} s ({} queued)", e, d, self.outbox.depth().unwrap_or(0));
                    self.stop_event.wait(d);
                }
            }
        }
    }

    // heartbeat and commands -------------------------------------------------------------------------

    pub fn capabilities(&self) -> Value {
        let info = self.monitor.heartbeat_info();
        let conn = info.get("connected").and_then(Value::as_bool).unwrap_or(false);
        let cfg = self.config();
        // turned off on the website: connected, with no job (v1.12)
        let role = self.scanner_role().filter(|r| r != "off");
        let channels: Vec<Value> = cfg.get("channels").and_then(Value::as_array).into_iter().flatten()
            .map(|c| c.get("ch").cloned().unwrap_or(Value::Null)).collect();
        let pick = |v: Value| if conn { v } else { Value::Null };
        let scanner = json!({
            "type": "scanner", "vendor": pick(json!("Uniden")),
            "product": info.get("model").cloned().unwrap_or(Value::Null),
            "firmware": info.get("firmware").cloned().unwrap_or(Value::Null), "connected": conn,
            "role": pick(json!(role)), "bands_mhz": pick(json!(BANDS_MHZ)), "channels": pick(json!(channels)),
            "simultaneous": false, "squelch_free": false, "audio": false,
            "scanner_id": self.scanner_id(),
        });
        let sdrs = self.sdrs();
        let vhf_tx = sdrs.iter().any(|s| s.with(|x| x.connected && x.role == "vhf"
            && x.transcriber.as_ref().is_some_and(|t| t.other_calls())));
        let eot_decode = sdrs.iter().any(|s| s.with(|x| x.connected && x.role == "eot"));
        let detector_reports = self.transcriber().and_then(|t| t.templates()).is_some_and(|l| !l.is_empty());
        let mut receivers = vec![scanner];
        // with automatic roles, only the dongles plugged in now: one thrown away stops showing on
        // the website (its job and the choice by serial stay here, for a replacement)
        let present = lock(&self.present_serials).clone();
        receivers.extend(sdrs.iter().filter_map(|s| {
            let serial = s.with(|x| x.serial.clone());
            if self.assigner.is_some() && !present.contains(&serial) {
                return None;
            }
            let mut c = s.with(|x| x.capability());
            c.insert("dongle_id".into(), if serial.is_empty() { Value::Null } else { json!(self.dongle_id(&serial)) });
            Some(Value::Object(c))
        }));
        // plugged-in dongles without a role, so the owner can give them one on the website
        for serial in self.unused_dongles() {
            receivers.push(json!({"type": "sdr", "connected": true, "role": null, "dongle_id": self.dongle_id(&serial)}));
        }
        json!({
            "receivers": receivers,
            "features": {
                "simultaneous_voice": self.sdr_covers("voice"),
                "continuous_eot": self.sdr_covers("eot"),
                "continuous_hot": conn && role.as_deref() == Some("hot"),
                "transcription": vhf_tx,
                "eot_decode": eot_decode,
                // API contract v1.5: sends `detector` and never sets parsed.defects from speech
                "detector_reports": detector_reports,
            },
        })
    }

    pub fn clock_synced(&self) -> bool {
        match self.api.clock_synced() {
            Some(v) => v,
            None => self.ntp != Some(false),
        }
    }

    pub fn heartbeat_body(&self) -> Value {
        let now = (self.clock)();
        json!({
            "ts": crate::util::round_even(now, 1), "uptime_s": (now - self.started) as i64,
            "agent_version": AGENT_VERSION, "scanner": self.monitor.heartbeat_info(),
            "outbox_depth": self.outbox.depth().unwrap_or(0), "dropped_events": self.outbox.dropped().unwrap_or(0),
            "last_event_ts": lock(&self.last_event_ts).clone(), "clock_synced": self.clock_synced(),
            "capabilities": self.capabilities(),
        })
    }

    pub fn heartbeat_once(&self) -> Result<Value, ApiError> {
        let body = self.heartbeat_body();
        let resp = self.api.heartbeat(&body)?;
        self.set_reg_status("approved");
        let _ = self.outbox.ack_dropped(body["dropped_events"].as_i64().unwrap_or(0));
        let mut lh = Map::new();
        lh.insert("ts".into(), body["ts"].clone());
        if let Value::Object(r) = &resp {
            lh.extend(r.clone());
        }
        *lock(&self.last_heartbeat) = Value::Object(lh);
        if let Some(v) = resp.get("config_version").filter(|v| truthy(v)) {
            if Some(v) != self.config().get("config_version") {
                self.fetch_config()?;
            }
        }
        if resp.get("commands_pending").and_then(Value::as_f64).unwrap_or(0.0) > 0.0 {
            for cmd in self.api.get_commands()? {
                let _ = self.commands.0.send(cmd);
            }
        }
        Ok(resp)
    }

    pub fn heartbeat_loop(&self) {
        while !self.stop_event.is_set() {
            if !self.approved.wait(1.0) {
                continue;
            }
            if let Err(e) = self.heartbeat_once() {
                self.handle_api_error(&e);
                log::warn!("heartbeat failed: {e}");
            }
            let every = self.config().get("heartbeat_interval_s").and_then(Value::as_f64).unwrap_or(60.0);
            // sooner when the dongles change, so the headend (and the website) see them at once
            let until = (self.clock)() + every;
            while !self.stop_event.is_set() && !self.hb_wake.is_set() && (self.clock)() < until {
                self.hb_wake.wait(1.0);
            }
            self.hb_wake.clear();
        }
    }

    pub fn run_command(&self, cmd: &Value) -> Value {
        CommandRunner::run(self, cmd)
    }

    pub fn command_loop(&self) {
        while !self.stop_event.is_set() {
            let Ok(cmd) = self.commands.1.recv_timeout(std::time::Duration::from_secs(1)) else { continue };
            let result = self.run_command(&cmd);
            let id = value_str_opt(cmd.get("id"));
            for attempt in 0..5 {
                match self.api.post_command_result(&id, &result) {
                    Ok(_) => break,
                    Err(e) => {
                        log::warn!("could not send result of {}: {}", id, e);
                        self.stop_event.wait(5.0 * (attempt + 1) as f64);
                    }
                }
            }
        }
    }

    // scanner -------------------------------------------------------------------------------------

    fn scanner_connected(&self, monitor: &ScannerMonitor) -> Result<(), ScannerError> {
        checks::recover_after_crash(monitor, &self.state_dir)?;
        self.focus.recover()?;
        lock(&self.sched).plan_synced_version = Value::Null;
        Ok(())
    }

    pub fn scanner_tick(&self) -> Result<(), String> {
        let m = &self.monitor;
        // a scanner plugged in or removed goes to the headend at once, as dongles do
        let id = self.scanner_id();
        if *lock(&self.last_scanner_id) != id {
            *lock(&self.last_scanner_id) = id;
            self.hb_wake.set();
        }
        if !m.connected() {
            return Ok(());
        }
        self.focus.tick().map_err(|e| e.msg().to_string())?;
        let cfg = self.config();
        let version = cfg.get("config_version").cloned().unwrap_or(Value::Null);
        let synced = lock(&self.sched).plan_synced_version.clone();
        if m.idle() && synced != version {
            match checks::plan_sync(m, &cfg, &self.state_dir) {
                Ok(rep) => {
                    lock(&self.sched).plan_synced_version = version;
                    if let Some(rep) = rep.filter(truthy) {
                        self.report("scan_setup", rep);
                    }
                }
                Err(e) => log::warn!("scanner plan sync failed: {}", e.msg()),
            }
        }
        let setting = self.settings.str("scanner_role").unwrap_or("auto").to_string();
        let website = (setting == "auto").then(|| self.website_scanner_job(&cfg)).flatten();
        let (role, why) = match website {
            Some(job) => (job, "chosen on the website".to_string()),
            None => desired_role(&setting, self.sdr_covers("eot"), self.sdr_covers("hot")),
        };
        *lock(&self.scanner_job_from) = if setting != "auto" { "receiver.json" }
            else if why == "chosen on the website" { "website" } else { "automatic" }.into();
        let mut roles = lock(&self.roles);
        if let Err(e) = roles.tick(&role, &why, &cfg, false) {
            log::warn!("could not apply scanner role {}: {}", role, e.msg());
        }
        *lock(&self.scanner_role) = roles.role.clone();
        Ok(())
    }

    // reference, radio checks, signal log ---------------------------------------------------------

    pub fn fresh_reference(&self, max_age: f64) -> Option<(Value, Arc<SdrHandle>)> {
        let now = (self.clock)();
        for s in self.sdrs() {
            let r = s.with(|x| if x.connected { x.reference.clone() } else { None });
            if let Some(r) = r.filter(truthy) {
                if now - r.get("ts").and_then(Value::as_f64).unwrap_or(f64::NEG_INFINITY) <= max_age {
                    return Some((r, s));
                }
            }
        }
        None
    }

    fn ref_name(r: &Value) -> String {
        match r.get("name").filter(|n| truthy(n)) {
            Some(n) => value_str(n),
            None => format!("{:.4} MHz", r.get("freq").and_then(Value::as_f64).unwrap_or(0.0)),
        }
    }

    pub fn watch_reference(&self) -> Result<(), String> {
        let now = (self.clock)();
        if self.monitor.connected() {
            let mut s = lock(&self.sched);
            s.ref_weak_since = None;
            s.ref_failing = false;
            return Ok(());
        }
        let Some((r, _)) = self.fresh_reference(10.0) else { return Ok(()) };
        let name = Self::ref_name(&r);
        let snr = r.get("snr_db").and_then(Value::as_f64).unwrap_or(f64::NAN);
        let mut s = lock(&self.sched);
        if snr < REF_WEAK_DB {
            let since = *s.ref_weak_since.get_or_insert(now);
            if now - since >= REF_WEAK_FOR_S && s.ref_fail_sent.is_none_or(|t| now - t >= REF_REPEAT_S) {
                s.ref_fail_sent = Some(now);
                s.ref_failing = true;
                drop(s);
                self.report("radio_check", checks::radio_check_sdr(&r, &name));
                log::warn!("radio check failed: {name} only {snr:+.0} dB on the SDR");
            }
        } else {
            s.ref_weak_since = None;
            let failing = s.ref_failing;
            s.ref_failing = false;
            s.ref_fail_sent = None;
            drop(s);
            if failing {
                self.report("radio_check", checks::radio_check_sdr(&r, &name));
                log::info!("radio check ok again: {name} {snr:+.0} dB");
            }
        }
        Ok(())
    }

    pub fn radio_check(&self, send: bool) -> Result<Value, ScannerError> {
        let cfg = self.config();
        let ref_cfg = cfg.get("reference").filter(|r| truthy(r)).cloned().unwrap_or_else(|| json!({}));
        let data = if self.monitor.connected() && truthy(&ref_cfg) {
            let data = checks::radio_check_scanner(&self.monitor, &self.state_dir, &ref_cfg, &self.monitor.timing())?;
            if self.focus.active().is_none() {
                match checks::scan_setup_check(&self.monitor, &cfg) {
                    Ok(setup) => {
                        if setup.get("fixed").is_some_and(truthy) || setup.get("problems").is_some_and(truthy) {
                            self.report("scan_setup", setup);
                        }
                    }
                    Err(e) => log::warn!("scan setup check failed: {}", e.msg()),
                }
            }
            data
        } else if let Some((r, _)) = self.fresh_reference(10.0) {
            let name = r.get("name").filter(|n| truthy(n)).map(value_str).unwrap_or_else(|| "reference".into());
            checks::radio_check_sdr(&r, &name)
        } else {
            let mut err = "no scanner or SDR connected".to_string();
            let vhf = self.sdrs().into_iter().find(|s| s.with(|x| x.connected));
            if let Some(v) = vhf {
                let f = ref_cfg.get("freq").and_then(Value::as_f64).unwrap_or(0.0);
                if let Err(e) = v.with(|x| x.measure(f)) {
                    err = e;
                }
            }
            json!({"freq": ref_cfg.get("freq").cloned().unwrap_or(Value::Null), "rssi": null, "squelch_open": null,
                   "ok": false, "error": err})
        };
        if send {
            self.report("radio_check", data.clone());
        }
        let ok = data.get("ok").is_some_and(truthy);
        log::info!("radio check: {}", if ok { "ok".into() } else { format!("failed ({})", value_str_opt(data.get("error"))) });
        Ok(data)
    }

    pub fn radio_check_tick(&self) -> Result<(), String> {
        let now = (self.clock)();
        if now < lock(&self.sched).next_radio_check {
            return Ok(());
        }
        if self.monitor.connected() && !self.monitor.idle() {
            lock(&self.sched).next_radio_check = now + RADIO_CHECK_RETRY_S;
            return Ok(());
        }
        if let Err(e) = self.radio_check(true) {
            log::warn!("radio check failed to run: {}", e.msg());
            lock(&self.sched).next_radio_check = now + RADIO_CHECK_RETRY_S;
            return Ok(());
        }
        let every = self.config().get("radio_check_interval_s").and_then(Value::as_f64).unwrap_or(1800.0);
        lock(&self.sched).next_radio_check = now + every;
        Ok(())
    }

    pub fn signal_tick(&self) -> Result<(), String> {
        let now = (self.clock)();
        let r = self.fresh_reference(10.0);
        let mut s = lock(&self.sched);
        if let Some((r, _)) = &r {
            s.signal_samples.push((now, r.get("snr_db").and_then(Value::as_f64).unwrap_or(f64::NAN)));
        }
        if now < s.next_signal_log {
            return Ok(());
        }
        s.next_signal_log = now + SIGNAL_LOG_S;
        let mut vals: Vec<f64> = s.signal_samples.iter().filter(|(t, _)| now - t <= SIGNAL_LOG_S).map(|(_, v)| *v).collect();
        s.signal_samples.clear();
        drop(s);
        let cfg = self.config();
        let name = cfg.get("reference").and_then(|r| r.get("name")).filter(|n| truthy(n)).map(value_str)
            .unwrap_or_else(|| "reference".into());
        let mut parts = vec![];
        if !vals.is_empty() {
            vals.sort_by(|a, b| a.total_cmp(b));
            parts.push(format!("signal: {} {:+.0} dB (min {:+.0}, max {:+.0} over {:.0} s)", name, vals[vals.len() / 2],
                               vals[0], vals[vals.len() - 1], SIGNAL_LOG_S));
        } else {
            parts.push(format!("signal: {name} not measured"));
        }
        for h in self.sdrs() {
            let (role, fl) = h.with(|x| (x.role.clone(), x.floors()));
            if !fl.is_empty() {
                let lo = fl.iter().cloned().fold(f64::INFINITY, f64::min);
                let hi = fl.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
                parts.push(format!("{role} floor {lo:.0}..{hi:.0} dB"));
            }
        }
        log::info!("{}", parts.join("; "));
        Ok(())
    }

    /// Every 2 s: which dongles are plugged in (sysfs), and automatic roles for new ones.
    pub fn usb_check_tick(&self, force: bool) {
        let now = (self.clock)();
        {
            let mut s = lock(&self.sched);
            if !force && now < s.next_usb_check {
                return;
            }
            s.next_usb_check = now + USB_CHECK_S;
        }
        let Some(usb) = (self.usb_lister)() else {
            // no sysfs: fall back to librtlsdr every 10 s
            self.sdr_scan_tick(force);
            return;
        };
        let serial_of = |d: &Value| d.get("serial").map(value_str).unwrap_or_default();
        if self.assigner.is_some() {
            self.apply_assignment(&usb.iter().map(serial_of).collect::<Vec<_>>());
        }
        let ports: Vec<(String, Option<String>)> = usb.iter().filter(|d| !serial_of(d).is_empty())
            .map(|d| (serial_of(d), d.get("port").and_then(Value::as_str).map(String::from))).collect();
        for h in self.sdrs() {
            h.with(|s| {
                if !s.serial.is_empty() {
                    // a map: the last entry for a serial wins
                    let hit = ports.iter().rev().find(|(ser, _)| *ser == s.serial);
                    s.set_plugged_in(hit.is_some(), hit.and_then(|(_, p)| p.as_deref()));
                }
            });
        }
    }

    /// Automatic roles through librtlsdr's dongle list, where sysfs isn't available.
    pub fn sdr_scan_tick(&self, force: bool) {
        if self.assigner.is_none() {
            return;
        }
        let now = (self.clock)();
        {
            let mut s = lock(&self.sched);
            if !force && now < s.next_sdr_scan {
                return;
            }
            s.next_sdr_scan = now + SDR_SCAN_S;
        }
        let devices = match (self.lister)() {
            Ok(d) => d,
            Err(e) => {
                let mut s = lock(&self.sched);
                if s.sdr_scan_error.as_deref() != Some(e.0.as_str()) {
                    log::warn!("cannot list the dongles: {e}");
                }
                s.sdr_scan_error = Some(e.0);
                return;
            }
        };
        lock(&self.sched).sdr_scan_error = None;
        let serials: Vec<String> = devices.iter()
            .map(|d| d.get("serial").filter(|v| truthy(v)).map(value_str).unwrap_or_default()).collect();
        self.apply_assignment(&serials);
    }

    /// A dongle's name for the headend (contract v1.11): stable for this receiver, and not its
    /// serial, which receivers never send (v1.1).
    pub fn dongle_id(&self, serial: &str) -> String {
        use sha2::{Digest, Sha256};
        let device = self.device_id().unwrap_or_default();
        let h = Sha256::digest(format!("{device}:{serial}").as_bytes());
        format!("sdr-{}", h.iter().take(4).map(|b| format!("{b:02x}")).collect::<String>())
    }

    /// The connected scanner's name for the headend (contract v1.12): a hash of the device ID and
    /// the scanner cable's USB serial, or of its model when there is none. Never the serial.
    pub fn scanner_id(&self) -> Option<String> {
        use sha2::{Digest, Sha256};
        let info = self.monitor.local_info();
        if info.get("connected").and_then(Value::as_bool) != Some(true) {
            return None;
        }
        let what = match info.get("cable_serial").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            Some(serial) => serial.to_string(),
            // the only scanner this receiver drives, so its place is always the first
            None => format!("model {}#0", value_str_opt(info.get("model"))),
        };
        let device = self.device_id().unwrap_or_default();
        let h = Sha256::digest(format!("{device}:{what}").as_bytes());
        Some(format!("scn-{}", h.iter().take(4).map(|b| format!("{b:02x}")).collect::<String>()))
    }

    /// The website's job for the connected scanner (`scanner_roles` in /config), if it names one.
    fn website_scanner_job(&self, cfg: &Value) -> Option<String> {
        let id = self.scanner_id()?;
        let job = cfg.get("scanner_roles")?.get(&id)?.as_str()?;
        if ["scan", "eot", "hot", "off"].contains(&job) {
            Some(job.to_string())
        } else {
            log::warn!("the headend gave the scanner an unknown job {job:?}; it stays automatic");
            None
        }
    }

    /// The headend's job for each plugged-in dongle it names (`sdr_roles` in /config), by serial.
    /// Jobs this receiver has no role for are left out.
    fn chosen_roles(&self, present: &[String]) -> std::collections::BTreeMap<String, String> {
        let cfg = self.config();
        let Some(map) = cfg.get("sdr_roles").and_then(Value::as_object) else { return Default::default() };
        let roles: Vec<String> = self.sdrs().iter().map(|h| h.with(|s| s.role.clone())).collect();
        present.iter().filter_map(|serial| {
            let role = map.get(&self.dongle_id(serial))?.as_str()?;
            (role == "off" || roles.iter().any(|r| r == role)).then(|| (serial.clone(), role.to_string()))
        }).collect()
    }

    /// Plugged-in dongles that have no role: spares, and ones the headend turned off.
    fn unused_dongles(&self) -> Vec<String> {
        let Some(assigner) = &self.assigner else { return vec![] };
        let a = lock(assigner);
        lock(&self.present_serials).iter()
            .filter(|s| !a.assigned.iter().any(|x| x.as_deref() == Some(s.as_str())))
            .cloned().collect()
    }

    /// Plugged-in dongles the website turned off.
    fn dongles_off(&self) -> Vec<String> {
        let Some(assigner) = &self.assigner else { return vec![] };
        lock(assigner).chosen.iter().filter(|(_, r)| *r == "off").map(|(s, _)| s.clone()).collect()
    }

    /// Serial to website ID for each plugged-in dongle, for the status page.
    fn present_dongle_ids(&self) -> Map<String, Value> {
        lock(&self.present_serials).iter().map(|s| (s.clone(), json!(self.dongle_id(s)))).collect()
    }

    /// Give each role a connected dongle, and say which antenna it needs.
    pub fn apply_assignment(&self, serials: &[String]) {
        let Some(assigner) = &self.assigner else { return };
        let present: Vec<String> = serials.iter().filter(|s| !s.is_empty()).cloned().collect();
        let kit_changed = {
            let mut p = lock(&self.present_serials);
            let changed = *p != present;
            *p = present.clone();
            changed
        };
        let chosen = self.chosen_roles(&present);
        let mut a = lock(assigner);
        let choice_changed = a.chosen != chosen;
        if choice_changed {
            for (serial, role) in &chosen {
                log::info!("the headend gives dongle {} (serial {serial}) the {role} job", self.dongle_id(serial));
            }
        }
        a.chosen = chosen;
        let (dups, spare) = (a.duplicates.clone(), a.spare.clone());
        let changes = a.update(serials);
        if kit_changed || !changes.is_empty() {
            self.hb_wake.set();
        }
        let sdrs = self.sdrs();
        for (i, old, new) in changes {
            let Some(h) = sdrs.get(i) else { continue };
            let (name, antenna) = h.with(|s| {
                // a dongle moved to another job is still streaming here: let it go, so the other
                // role can open it (and this one opens its new dongle)
                if s.streaming {
                    if let Some(dev) = &s.dev {
                        dev.cancel_read_async();
                    }
                }
                s.serial = new.clone();
                s.plugged_in = None;
                (s.name.clone(), s.antenna.clone())
            });
            if new.is_empty() {
                log::info!("{name} has no dongle now: the headend gave serial {} another job",
                           old.unwrap_or_default());
                continue;
            }
            let replacing = match old.filter(|o| !o.is_empty()) {
                Some(o) if choice_changed => format!(" (instead of {o}, as chosen on the website)"),
                Some(o) => format!(" (replacing {o}, missing for {:.0} s)", a.release_after_s),
                None => String::new(),
            };
            log::info!("{name} is the dongle with serial {new}{replacing}; connect {antenna} to it");
        }
        if !a.duplicates.is_empty() && a.duplicates != dups {
            log::warn!("dongles with serial {} can't be told apart, so they are not used; give each its own serial \
                        with `rtl_eeprom -d <index> -s <serial>` while the service is stopped, then replug it",
                       a.duplicates.join(", "));
        }
        if !a.spare.is_empty() && a.spare != spare {
            log::info!("dongle {} not needed: every role already has one",
                       a.spare.iter().map(|s| format!("serial {s}")).collect::<Vec<_>>().join(", "));
        }
    }

    /// One reading a minute for the status page: reference SNR and each dongle's floors.
    pub fn history_signal_tick(&self) -> Result<(), String> {
        let now = (self.clock)();
        {
            let mut s = lock(&self.sched);
            if now < s.next_history_signal {
                return Ok(());
            }
            s.next_history_signal = now + HISTORY_SIGNAL_S;
        }
        let r = self.fresh_reference(HISTORY_SIGNAL_S);
        let sdrs: Vec<Value> = self.sdrs().iter().map(|h| h.with(|s| {
            let mut fl = s.floors();
            fl.sort_by(|a, b| a.total_cmp(b));
            let floor = if fl.is_empty() { Value::Null } else { json!(crate::util::round_even(fl[fl.len() / 2], 1)) };
            json!({"role": s.role, "name": s.name, "connected": s.connected, "floor_db": floor})
        })).collect();
        let reading = json!({"ts": crate::util::round_even(now, 1),
                             "ref_snr_db": r.map(|(r, _)| r.get("snr_db").cloned().unwrap_or(Value::Null)).unwrap_or(Value::Null),
                             "sdrs": sdrs});
        self.history.add_signal(&reading).map_err(|e| format!("{e:?}"))
    }

    // transcription test --------------------------------------------------------------------------

    pub fn transcription_test_tick(&self) -> Result<(), String> {
        let trigger = self.state_dir.join(TT_TRIGGER);
        let now = (self.clock)();
        let timed_out = {
            let mut p = lock(&self.tt_pending);
            match *p {
                Some(t) if now - t > TT_WAIT_S => {
                    *p = None;
                    true
                }
                _ => false,
            }
        };
        if timed_out {
            self.tt_result(None, Some("no reference audio within 25 s".into()));
        }
        if !trigger.exists() {
            return Ok(());
        }
        let _ = std::fs::remove_file(&trigger);
        if self.tt_pending().is_some() {
            return Ok(());
        }
        if self.transcriber().is_none() {
            self.tt_result(None, Some("transcription is turned off in receiver.json".into()));
            return Ok(());
        }
        let vhf = self.sdrs().into_iter().find(|s| s.with(|x| x.role == "vhf"));
        let me = self.me.clone();
        // pending before the tap can finish (it may, inside a block, before this returns)
        *lock(&self.tt_pending) = Some(now);
        let ok = vhf.is_some_and(|v| v.with(|x| x.record_reference(TT_SECONDS, Box::new(move |audio| {
            if let Some(a) = me.upgrade() {
                a.tt_audio(audio);
            }
        }))));
        if !ok {
            *lock(&self.tt_pending) = None;
            self.tt_result(None, Some("the reference station is not in a connected VHF SDR window".into()));
            return Ok(());
        }
        log::info!("transcription test: recording {:.0} s of {}", TT_SECONDS,
                   value_str_opt(self.config().get("reference").and_then(|r| r.get("name"))));
        Ok(())
    }

    fn tt_audio(&self, audio: Vec<f32>) {
        if lock(&self.tt_pending).take().is_none() {
            return;
        }
        let Some(tr) = self.transcriber() else { return };
        let me = self.me.clone();
        let tr2 = tr.clone();
        tr.submit(audio, Box::new(move |r| {
            if let Some(a) = me.upgrade() {
                let err = if r.is_some() { None } else {
                    Some(tr2.last_reject().and_then(|l| l.get("reason").filter(|v| truthy(v)).map(value_str))
                        .unwrap_or_else(|| "no transcript".into()))
                };
                a.tt_result(r, err);
            }
        }), "test", None);
    }

    fn tt_result(&self, result: Option<Map<String, Value>>, error: Option<String>) {
        let line = match &result {
            Some(r) => format!("transcription test: \"{}\" ({}%)", value_str_opt(r.get("text")),
                               crate::util::round_int(r.get("confidence").and_then(Value::as_f64).unwrap_or(0.0) * 100.0)),
            None => format!("transcription test failed: {}", error.clone().unwrap_or_else(|| "None".into())),
        };
        log::info!("{line}");
        let data = json!({"type": "transcription_test", "ts": (self.clock)(), "ok": result.is_some(),
                          "result": result, "error": error, "line": line});
        if let Err(e) = std::fs::write(self.state_dir.join(TT_RESULT), json_dumps_indent(&data, 2) + "\n") {
            log::warn!("could not write {TT_RESULT}: {e}");
        }
    }

    // status ---------------------------------------------------------------------------------------

    pub fn link_info(&self) -> Option<Value> {
        let lk = self.link()?;
        Some(json!({"user_code": link::format_user_code(&lk.user_code), "verification_uri": lk.verification_uri,
                    "verification_uri_complete": lk.verification_uri_complete,
                    "expires_at": crate::util::round_even(lk.expires_at, 1)}))
    }

    pub fn status(&self) -> Value {
        let assigned = if self.auto_sdr { "automatic" } else { "receiver.json" };
        let sdrs = self.sdrs();
        let mut sdr_status = vec![];
        let (mut last_eot, mut last_tr): (Option<Value>, Option<Value>) = (None, None);
        let ts_of = |v: &Value| v.get("ts").and_then(Value::as_f64).unwrap_or(f64::NEG_INFINITY);
        for h in &sdrs {
            let (st, eot, tr) = h.with(|s| (s.status(), s.last_eot.clone(), s.last_transcript()));
            let mut st = st;
            let serial = st.get("serial").and_then(Value::as_str).unwrap_or("").to_string();
            let chosen = self.assigner.as_ref().is_some_and(|a| lock(a).chosen.contains_key(&serial));
            st.insert("assigned".into(), json!(if chosen { "website" } else { assigned }));
            st.insert("dongle_id".into(), if serial.is_empty() { Value::Null } else { json!(self.dongle_id(&serial)) });
            sdr_status.push(Value::Object(st));
            // max(): the first of equal ones
            if let Some(e) = eot.filter(truthy) {
                if last_eot.as_ref().is_none_or(|l| ts_of(&e) > ts_of(l)) {
                    last_eot = Some(e);
                }
            }
            if let Some(t) = tr.filter(truthy) {
                if last_tr.as_ref().is_none_or(|l| ts_of(&t) > ts_of(l)) {
                    last_tr = Some(t);
                }
            }
        }
        let (dups, spare) = self.assigner_info().unwrap_or_default();
        let recent: Vec<Value> = {
            let r = lock(&self.recent_events);
            r.iter().skip(r.len().saturating_sub(10)).cloned().collect()
        };
        let cfg = self.config();
        json!({
            "ts": (self.clock)(), "pid": std::process::id(), "agent_version": AGENT_VERSION,
            "simulate": self.simulate, "device_id": self.device_id(),
            "registration": self.reg_status(), "link": self.link_info(),
            "server_url": self.settings.get("server_url"), "config_version": cfg.get("config_version").cloned().unwrap_or(Value::Null),
            "sdrs": sdr_status, "scanner": self.monitor.local_info(),
            "sdr_assign": {"auto": self.auto_sdr, "duplicates": dups, "spare": spare,
                           "error": lock(&self.sched).sdr_scan_error.clone(),
                           // v1.11: dongles turned off on the website, and each plugged-in dongle's ID there
                           "off": self.dongles_off(), "dongle_ids": self.present_dongle_ids()},
            "scanner_role": self.scanner_role(), "scanner_job_from": lock(&self.scanner_job_from).clone(),
            "scanner_id": self.scanner_id(), "focus": self.focus.active(),
            "auto_eot_runs": lock(&self.auto_eot).runs(),
            "outbox_depth": self.outbox.depth().unwrap_or(0), "dropped_events": self.outbox.dropped().unwrap_or(0),
            "uploaded": self.uploaded(),
            "last_events": recent, "last_eot": last_eot,
            "last_transcript": last_tr,
            "last_no_transcript": self.transcriber().and_then(|t| t.last_reject()),
            "api": self.api.last_results(), "clock_offset_s": self.api.offset(),
            "clock_synced": self.clock_synced(), "last_heartbeat": lock(&self.last_heartbeat).clone(),
        })
    }

    pub fn write_status(&self, stopped: bool) -> Result<(), String> {
        let path = self.state_dir.join("status.json");
        let tmp = self.state_dir.join("status.json.tmp");
        let mut data = self.status();
        data["stopped"] = json!(stopped);
        std::fs::write(&tmp, json_dumps_indent(&data, 2) + "\n").map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
    }

    // scheduler -------------------------------------------------------------------------------------

    pub fn scheduler_tick(&self) {
        type Step<'a> = (&'static str, Box<dyn Fn() -> Result<(), String> + 'a>);
        let mut steps: Vec<Step> = vec![];
        steps.push(("scanner_tick", Box::new(|| self.scanner_tick())));
        steps.push(("watch_reference", Box::new(|| self.watch_reference())));
        steps.push(("signal_tick", Box::new(|| self.signal_tick())));
        steps.push(("history_signal_tick", Box::new(|| self.history_signal_tick())));
        steps.push(("transcription_test_tick", Box::new(|| self.transcription_test_tick())));
        steps.push(("link_trigger_tick", Box::new(|| self.link_trigger_tick())));
        steps.push(("radio_check_tick", Box::new(|| self.radio_check_tick())));
        steps.push(("write_status", Box::new(|| self.write_status(false))));
        for (name, step) in steps {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(step)) {
                Ok(Ok(())) => {}
                Ok(Err(e)) => log::error!("scheduler step {name} failed: {e}"),
                Err(_) => log::error!("scheduler step {name} failed"),
            }
        }
    }

    pub fn usb_loop(&self) {
        while !self.stop_event.is_set() {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.usb_check_tick(true))).is_err() {
                log::error!("USB check failed");
            }
            self.stop_event.wait(USB_CHECK_S);
        }
    }

    pub fn scheduler_loop(&self) {
        while !self.stop_event.is_set() {
            self.scheduler_tick();
            self.stop_event.wait(SCHED_S);
        }
    }

    // run --------------------------------------------------------------------------------------------

    fn spawn(&self, name: &str, f: fn(&Agent)) {
        let Some(me) = self.arc() else { return };
        match std::thread::Builder::new().name(name.into()).spawn(move || f(&me)) {
            Ok(t) => lock(&self.threads).push(t),
            Err(e) => log::error!("could not start thread {name}: {e}"),
        }
    }

    pub fn start(&self) {
        let ident = self.identity();
        log::info!("RailSpotting receiver {} starting as {} ({} on {}){}", AGENT_VERSION,
                   self.device_id().unwrap_or_else(|| "an unlinked receiver".into()), value_str_opt(ident.get("mac")),
                   value_str_opt(ident.get("interface")), if self.simulate { " in simulation" } else { "" });
        if self.ntp == Some(false) {
            log::warn!("timedatectl says the clock is not NTP synchronized; signing needs it within 300 s of the server");
        }
        if self.sdrs().is_empty() {
            log::warn!("no SDR dongles configured in receiver.json (sdr.devices)");
        }
        self.usb_check_tick(true);
        let cfg = self.config();
        let chans = cfg.get("channels").and_then(Value::as_array).cloned().unwrap_or_default();
        for s in self.sdrs() {
            s.with(|x| x.set_plan(&chans, cfg.get("reference").filter(|r| !r.is_null())));
            s.start();
        }
        if let Some(tr) = self.transcriber() {
            tr.start();
            self.check_detectors(&cfg);
        }
        let monitor = self.monitor.start();
        lock(&self.threads).push(monitor);
        if let Some(w) = lock(&self.web).as_mut() {
            w.start();
        }
        if self.simulate {
            if let Some(w) = &self.world {
                let t = railspotting_dsp::sim::Traffic::new(w.clone(), &cfg, self.clock.clone());
                *lock(&self.traffic) = Some(t.spawn());
            }
        }
        self.spawn("registration_loop", |a| a.registration_loop(false));
        self.spawn("uploader_loop", Agent::uploader_loop);
        self.spawn("heartbeat_loop", Agent::heartbeat_loop);
        self.spawn("command_loop", Agent::command_loop);
        self.spawn("scheduler_loop", Agent::scheduler_loop);
        self.spawn("usb_loop", Agent::usb_loop);
    }

    pub fn stop(&self) {
        self.stop_event.set();
        self.reg_wake.set();
        for s in self.sdrs() {
            s.stop();
        }
        self.monitor.stop();
        if let Some(w) = lock(&self.web).as_mut() {
            w.stop();
        }
        if let Some(tr) = self.transcriber() {
            tr.stop();
        }
        if let Some(mut t) = lock(&self.traffic).take() {
            t.stop();
        }
    }

    /// Until SIGTERM or SIGINT; then stop and write the final status. The CLI uses
    /// `handle::Engine` instead; this stays for tools that drive an agent directly.
    pub fn run_forever(&self) {
        self.start();
        let term = Arc::new(std::sync::atomic::AtomicBool::new(false));
        for sig in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
            let _ = signal_hook::flag::register(sig, term.clone());
        }
        while !term.load(std::sync::atomic::Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        log::info!("stopping");
        self.stop();
        let _ = self.write_status(true);
    }

    /// Wait for the agent's own threads (after stop), at most `timeout_s`.
    pub fn join(&self, timeout_s: f64) {
        let threads: Vec<_> = lock(&self.threads).drain(..).collect();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs_f64(timeout_s);
        for t in threads {
            while !t.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            if t.is_finished() {
                let _ = t.join();
            }
        }
    }

    pub fn factory(&self) -> Option<DeviceFactory> {
        self.factory.clone()
    }

    pub fn set_event_hook(&self, hook: Option<EventHook>) {
        *self.event_hook.write().unwrap_or_else(|p| p.into_inner()) = hook;
    }

    /// Send what is queued, for at most `timeout_s`. Only when linked: an unlinked receiver
    /// can't upload, and the outbox keeps everything for the next start anyway.
    pub fn flush_outbox(&self, timeout_s: f64) {
        if !self.approved.is_set() || self.outbox.depth().unwrap_or(0) == 0 {
            return;
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs_f64(timeout_s.max(0.0));
        while std::time::Instant::now() < deadline && self.outbox.depth().unwrap_or(0) > 0 {
            match self.upload_once() {
                Ok(true) => {}
                Ok(false) => break,
                Err(e) => {
                    log::warn!("upload before stopping failed: {e}; {} events stay queued", self.outbox.depth().unwrap_or(0));
                    break;
                }
            }
        }
    }

    /// Stop everything and wait for it, so the dongles and the scanner port are free again:
    /// radios first (no new events), then a short outbox flush, then the rest.
    pub fn shutdown(&self, flush_s: f64, join_s: f64) {
        for s in self.sdrs() {
            s.stop();
        }
        self.monitor.stop();
        self.flush_outbox(flush_s);
        self.stop();
        self.join(join_s);
        // a dongle stuck in a USB read must not hold up quitting
        let sdrs = self.sdrs();
        let (tx, rx) = crossbeam_channel::bounded::<()>(1);
        let joiner = std::thread::Builder::new().name("sdr-join".into()).spawn(move || {
            for s in sdrs {
                s.join();
            }
            let _ = tx.send(());
        });
        if joiner.is_ok() && rx.recv_timeout(std::time::Duration::from_secs_f64(join_s.max(0.0))).is_err() {
            log::warn!("a dongle did not stop within {join_s:.0} s");
        }
        self.monitor.close();
        let _ = self.write_status(true);
    }
}

/// The scanner simulator's signal comes from the same world as the SDRs'.
struct WorldRssi(Arc<World>);

impl railspotting_scanner::fake::RssiSource for WorldRssi {
    fn scanner_rssi(&self, freq: Option<f64>, now: f64) -> i64 {
        self.0.scanner_rssi(freq, now)
    }
}

impl CommandHost for Agent {
    fn monitor(&self) -> &ScannerMonitor {
        &self.monitor
    }
    fn state_dir(&self) -> PathBuf {
        self.state_dir.clone()
    }
    fn config(&self) -> Value {
        Agent::config(self)
    }
    fn radio_check(&self, send: bool) -> Result<Value, ScannerError> {
        Agent::radio_check(self, send)
    }
    fn report(&self, rtype: &str, data: Value) {
        Agent::report(self, rtype, data)
    }
    fn timing(&self) -> Timing {
        self.monitor.timing()
    }
}
