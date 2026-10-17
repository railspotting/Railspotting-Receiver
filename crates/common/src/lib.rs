//! Shared settings, the built-in channel plan, the cached headend config and logging.
//!
//! Settings stay a JSON tree (`receiver.json` merged over the defaults), so every crate reads
//! the same keys and existing `receiver.json` files keep working.

use serde_json::{json, Map, Value};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub mod paths;

/// Reported to the headend as `agent_version`.
pub const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const NOMINAL_NOISE_VHF: i64 = 385;
pub const NOMINAL_NOISE_UHF: i64 = 230;
pub const UHF_MHZ: f64 = 400.0;
pub const PLACEHOLDER_SERIALS: [&str; 2] = ["00000001", "00000002"];

/// The v1.1 RSSI scale's nominal noise for a frequency.
pub fn nominal_noise(freq_mhz: f64) -> i64 {
    if freq_mhz >= UHF_MHZ { NOMINAL_NOISE_UHF } else { NOMINAL_NOISE_VHF }
}

pub fn builtin_config() -> Value {
    json!({
        "config_version": "builtin",
        "heartbeat_interval_s": 60,
        "radio_check_interval_s": 1800,
        "poll_interval_s": 0.25,
        "reference": {"name": "NOAA WXJ21 Paris Mountain", "freq": 162.55},
        "channels": [
            {"ch": 1, "freq": 160.905, "railroad": "NS", "label": "Greenville Dist. Road", "kind": "voice", "proximity": true},
            {"ch": 2, "freq": 160.245, "railroad": "NS", "label": "Greenville Dist. Dispatcher", "kind": "voice", "proximity": false},
            {"ch": 3, "freq": 161.19, "railroad": "NS", "label": "Yard", "kind": "voice", "proximity": false},
            {"ch": 4, "freq": 161.25, "railroad": "NS", "label": "Yard", "kind": "voice", "proximity": false},
            {"ch": 5, "freq": 160.95, "railroad": "NS", "label": "Charlotte Dist. Road", "kind": "voice", "proximity": true},
            {"ch": 6, "freq": 161.31, "railroad": "NS", "label": "Charlotte Dist. Dispatcher", "kind": "voice", "proximity": false},
            {"ch": 7, "freq": 161.1, "railroad": "CSX", "label": "Belton Sub Road", "kind": "voice", "proximity": true},
            {"ch": 8, "freq": 161.37, "railroad": "CSX", "label": "Belton Sub Dispatcher (AP)", "kind": "voice", "proximity": false},
            {"ch": 9, "freq": 457.9375, "railroad": null, "label": "EOT (end of train)", "kind": "telemetry", "proximity": true},
            {"ch": 10, "freq": 452.9375, "railroad": null, "label": "HOT (head of train)", "kind": "telemetry", "proximity": true}
        ],
        "upload": {"max_batch": 200, "flush_interval_s": 2}
    })
}

pub fn default_settings() -> Value {
    json!({
        "server_url": "https://api.railspotting.com",
        "ca_file": null,
        "serial_port": "auto",
        "baud": 57600,
        "label": null,
        "state_dir": "state",
        "log_level": "INFO",
        "scanner_role": "auto",
        "transcription": true,
        "transcribe_other_calls": false,
        "web": {"enabled": true, "bind": "0.0.0.0", "port": 8080},
        "auto_eot_focus": {"enabled": true, "minutes": 30, "eot_rssi_margin": 15, "hot_min_rssi": 0,
                           "road_trigger": true, "road_min_rssi": 420},
        "sdr": {
            "margin_db": 6.0,
            "min_windows": 2,
            "record_bursts": true,
            "record_keep": 500,
            "voice_clip_keep": 50,
            "auto_assign": true,
            "devices": []
        }
    })
}

/// Deep merge: objects merge key by key, anything else in `over` replaces `base`.
pub fn merge(base: &Value, over: &Value) -> Value {
    match (base, over) {
        (Value::Object(b), Value::Object(o)) => {
            let mut out: Map<String, Value> = b.clone();
            for (k, v) in o {
                let merged = match out.get(k) {
                    Some(bv) => merge(bv, v),
                    None => v.clone(),
                };
                out.insert(k.clone(), merged);
            }
            Value::Object(out)
        }
        _ => over.clone(),
    }
}

/// receiver.json merged over the defaults, plus where it was loaded from.
#[derive(Debug, Clone)]
pub struct Settings {
    pub raw: Value,
    pub base_dir: PathBuf,
    pub path: Option<PathBuf>,
}

impl Settings {
    pub fn from_value(raw: &Value, base_dir: impl Into<PathBuf>) -> Self {
        Settings { raw: merge(&default_settings(), raw), base_dir: base_dir.into(), path: None }
    }

    /// Load `path`; a missing file means defaults (logged).
    pub fn load(path: &Path) -> Result<Self, String> {
        let raw = if path.exists() {
            let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?
        } else {
            log::warn!("{} not found; using defaults", path.display());
            json!({})
        };
        let base = path.canonicalize().ok().and_then(|p| p.parent().map(Path::to_path_buf))
            .or_else(|| path.parent().map(Path::to_path_buf)).unwrap_or_else(|| PathBuf::from("."));
        let mut s = Settings::from_value(&raw, base);
        s.path = Some(path.to_path_buf());
        Ok(s)
    }

    pub fn get(&self, key: &str) -> &Value { self.raw.get(key).unwrap_or(&Value::Null) }
    pub fn str(&self, key: &str) -> Option<&str> { self.get(key).as_str() }
    pub fn bool(&self, key: &str, default: bool) -> bool { self.get(key).as_bool().unwrap_or(default) }

    pub fn state_dir(&self) -> PathBuf {
        let p = PathBuf::from(self.str("state_dir").unwrap_or("state"));
        if p.is_absolute() { p } else { self.base_dir.join(p) }
    }
    pub fn models_dir(&self) -> PathBuf {
        self.state_dir().parent().map(|p| p.join("models")).unwrap_or_else(|| PathBuf::from("models"))
    }
    pub fn credentials_path(&self) -> PathBuf { self.base_dir.join("credentials.json") }
    pub fn sdr(&self) -> &Value { self.get("sdr") }
    pub fn devices(&self) -> Vec<Value> {
        self.sdr().get("devices").and_then(Value::as_array).cloned().unwrap_or_default()
    }
}

/// The last channel plan from the headend, or the built-in one.
pub fn load_cached_config(state_dir: &Path) -> Value {
    let p = state_dir.join("config.json");
    if let Ok(text) = fs::read_to_string(&p) {
        if let Ok(cfg) = serde_json::from_str::<Value>(&text) {
            if cfg.get("channels").and_then(Value::as_array).is_some_and(|a| !a.is_empty()) {
                return cfg;
            }
        }
    }
    builtin_config()
}

pub fn save_cached_config(state_dir: &Path, cfg: &Value, etag: Option<&str>) -> std::io::Result<()> {
    fs::create_dir_all(state_dir)?;
    let mut data = cfg.clone();
    if let (Some(e), Some(obj)) = (etag, data.as_object_mut()) {
        obj.insert("_etag".into(), Value::String(e.into()));
    }
    write_atomic(&state_dir.join("config.json"), (serde_json::to_string_pretty(&data)? + "\n").as_bytes())
}

/// Write via a temporary file and rename, so readers never see half a file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
    }
    fs::rename(tmp, path)
}

pub fn plan_channel(cfg: &Value, freq: f64) -> Option<&Value> {
    plan_channel_tol(cfg, freq, 0.00125)
}

pub fn plan_channel_tol(cfg: &Value, freq: f64, tol: f64) -> Option<&Value> {
    cfg.get("channels")?.as_array()?.iter()
        .find(|c| c.get("freq").and_then(Value::as_f64).is_some_and(|f| (f - freq).abs() <= tol))
}

pub fn label_has(ch: &Value, word: &str) -> bool {
    ch.get("label").and_then(Value::as_str).unwrap_or("").to_lowercase().contains(&word.to_lowercase())
}

pub fn telemetry_channel<'a>(cfg: &'a Value, word: &str) -> Option<&'a Value> {
    cfg.get("channels")?.as_array()?.iter()
        .find(|c| c.get("kind").and_then(Value::as_str) == Some("telemetry") && label_has(c, word))
}

/// Seconds since the Unix epoch, as a float.
pub fn now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// Injectable clock, so tests can run with a fake one (`FakeClock`).
pub type Clock = std::sync::Arc<dyn Fn() -> f64 + Send + Sync>;

pub fn system_clock() -> Clock { std::sync::Arc::new(now) }

/// A fake clock for tests: starts at `t` and moves only when told.
#[derive(Clone)]
pub struct FakeClock(pub std::sync::Arc<std::sync::Mutex<f64>>);
impl FakeClock {
    pub fn new(t: f64) -> Self { FakeClock(std::sync::Arc::new(std::sync::Mutex::new(t))) }
    pub fn advance(&self, s: f64) { *self.0.lock().unwrap() += s; }
    pub fn get(&self) -> f64 { *self.0.lock().unwrap() }
    pub fn clock(&self) -> Clock { let c = self.0.clone(); std::sync::Arc::new(move || *c.lock().unwrap()) }
}

/// Log lines for journald: `LEVEL message`.
pub fn init_logging(level: &str) {
    init_logging_to(level, Box::new(std::io::stdout()), false);
}

/// Log lines to `out`. A log file has no journald to add the time, so `stamp` puts a UTC time
/// in front: `2026-10-12 18:51:15 LEVEL message`.
pub fn init_logging_to(level: &str, out: Box<dyn Write + Send>, stamp: bool) {
    struct Logger {
        filter: log::LevelFilter,
        out: std::sync::Mutex<Box<dyn Write + Send>>,
        stamp: bool,
    }
    impl log::Log for Logger {
        fn enabled(&self, m: &log::Metadata) -> bool { m.level() <= self.filter }
        fn log(&self, r: &log::Record) {
            if self.enabled(r.metadata()) {
                let lvl = match r.level() { log::Level::Warn => "WARNING", l => l.as_str() };
                let t = if self.stamp { format!("{} ", utc_text(now())) } else { String::new() };
                if let Ok(mut o) = self.out.lock() {
                    let _ = writeln!(o, "{t}{lvl} {}", r.args());
                    if self.stamp {
                        let _ = o.flush();
                    }
                }
            }
        }
        fn flush(&self) {
            if let Ok(mut o) = self.out.lock() {
                let _ = o.flush();
            }
        }
    }
    let filter = match level.to_ascii_uppercase().as_str() {
        "DEBUG" => log::LevelFilter::Debug, "WARNING" | "WARN" => log::LevelFilter::Warn,
        "ERROR" => log::LevelFilter::Error, _ => log::LevelFilter::Info,
    };
    let logger = Logger { filter, out: std::sync::Mutex::new(out), stamp };
    let _ = log::set_boxed_logger(Box::new(logger)).map(|()| log::set_max_level(filter));
}

/// `YYYY-MM-DD HH:MM:SS` in UTC for epoch seconds.
pub fn utc_text(ts: f64) -> String {
    let secs = ts.max(0.0) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // days since 1970-01-01 to a civil date (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_merge_and_paths() {
        let s = Settings::from_value(&json!({"sdr": {"margin_db": 8.0}, "state_dir": "st"}), "/opt/r");
        assert_eq!(s.sdr()["margin_db"], 8.0);
        assert_eq!(s.sdr()["min_windows"], 2);              // defaults kept beside the override
        assert_eq!(s.state_dir(), PathBuf::from("/opt/r/st"));
        assert_eq!(s.models_dir(), PathBuf::from("/opt/r/models"));
        assert_eq!(s.credentials_path(), PathBuf::from("/opt/r/credentials.json"));
        assert!(!s.bool("transcribe_other_calls", true));
    }

    #[test]
    fn plan_helpers() {
        let cfg = builtin_config();
        assert_eq!(plan_channel(&cfg, 160.9504).unwrap()["ch"], 5);
        assert!(plan_channel(&cfg, 160.96).is_none());
        assert_eq!(telemetry_channel(&cfg, "eot").unwrap()["ch"], 9);
        assert_eq!(nominal_noise(457.9375), 230);
        assert_eq!(nominal_noise(160.95), 385);
    }

    #[test]
    fn utc_text_dates() {
        assert_eq!(utc_text(0.0), "1970-01-01 00:00:00");
        assert_eq!(utc_text(951_782_400.0), "2000-02-29 00:00:00");
        assert_eq!(utc_text(1_791_831_075.0), "2026-10-12 18:51:15");
    }

    #[test]
    fn cached_config_round_trip() {
        let d = std::env::temp_dir().join(format!("rs-common-{}", std::process::id()));
        assert_eq!(load_cached_config(&d)["config_version"], "builtin");
        let mut cfg = builtin_config();
        cfg["config_version"] = json!("7");
        save_cached_config(&d, &cfg, Some("\"7\"")).unwrap();
        let back = load_cached_config(&d);
        assert_eq!(back["config_version"], "7");
        assert_eq!(back["_etag"], "\"7\"");
        let _ = fs::remove_dir_all(d);
    }
}
