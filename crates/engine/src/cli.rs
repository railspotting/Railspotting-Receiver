//! The command line: the argument parser and one cmd_* function per subcommand.

use crate::agent::{Agent, AgentOptions, Lister, INSTALLED_CONFIG, LINK_TRIGGER, TT_RESULT, TT_TRIGGER};
use crate::rtl::DriverError;
use crate::transcribe::Transcriber;
use crate::util::{float_repr, fmt_g, json_dumps, ljust, value_str, value_str_opt, rjust, truthy};
use clap::{Parser, Subcommand};
use railspotting_common::{load_cached_config, Settings, PLACEHOLDER_SERIALS};
use railspotting_detect::detectors::{assess, check_length_and_rhythm, features, Assessment, DetectorTemplates, CLIP_LEAD_S};
use railspotting_proto::keys;
use railspotting_store::assign::{auto_mode, role_configs, RoleAssigner, ROLES_FILE};
use railspotting_store::identity::save_identity;
use railspotting_store::outbox::Outbox;
use serde_json::{json, Map, Value};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Replayed detector events get IDs from this namespace and the original event ID.
pub const REPLAY_NAMESPACE: uuid::Uuid = uuid::uuid!("6f1c4e1e-6a0b-4a52-9d55-4d3c1f0a7b15");

#[derive(Parser, Debug)]
#[command(name = "railspotting-receiver", version,
          about = "RailSpotting receiver: hears railroad radio and reports to the headend")]
pub struct Args {
    /// path to receiver.json (default ./receiver.json)
    #[arg(long, env = "RAILSPOTTING_CONFIG", default_value = "receiver.json")]
    pub config: String,
    /// create a new key and forget the device ID; the receiver links again
    #[arg(long)]
    pub rekey: bool,
    /// delete device ID and key
    #[arg(long)]
    pub reset_identity: bool,
    #[command(subcommand)]
    pub cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// normal operation
    Run {
        /// fake radios with synthetic traffic
        #[arg(long)]
        simulate: bool,
    },
    /// link this receiver to an account (start again after a denial)
    Link,
    /// show what the running service is doing
    Status,
    /// list connected RTL-SDR dongles
    SdrList,
    /// record the reference station and transcribe it
    TestTranscription,
    /// templates for known defect detector announcements
    Detector {
        #[command(subcommand)]
        action: DetectorCmd,
    },
    /// ITC (220 MHz PTC): survey an area at setup, decode, record (uses a dongle directly; the
    /// running receiver decodes ITC by itself on a dongle with the itc role)
    Itc {
        #[command(subcommand)]
        action: crate::itc::ItcCmd,
    },
}

#[derive(Subcommand, Debug)]
pub enum DetectorCmd {
    /// learn an announcement from saved voice clips
    Add {
        /// for example ns-475.8
        name: String,
        /// exactly what the all-clear announcement says
        #[arg(long, required = true)]
        text: String,
        /// seconds where the detector's name and milepost end and 'no defects' begins; lets the
        /// receiver flag any message from this detector
        #[arg(long)]
        split: Option<f64>,
        /// seconds where the wording before the milepost number ends ('Norfolk Southern,
        /// milepost'); with it the number isn't matched, so the same kind of detector is
        /// recognized at any milepost
        #[arg(long)]
        opening_end: Option<f64>,
        /// clip IDs from state/voice_clips, or WAV files
        #[arg(required = true)]
        clips: Vec<String>,
    },
    /// show the templates
    List,
    /// show announcements kept from detectors that have no template yet (watched channels)
    Candidates,
    /// delete a template
    Remove { name: String },
    /// score clips against the templates
    Test {
        #[arg(required = true)]
        clips: Vec<String>,
    },
    /// detector events (API v1.5) from the saved clips of the last hours, for the headend's
    /// historical load
    Replay {
        #[arg(long, default_value_t = 12.0)]
        hours: f64,
        /// queue them for upload (default: show only)
        #[arg(long)]
        send: bool,
    },
}

fn now() -> f64 {
    railspotting_common::now()
}

fn read_json(p: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
}

/// A JSON value as the CLI prints it: strings quoted, lists as [a, b], objects as {'k': v}.
pub fn repr_value(v: &Value) -> String {
    match v {
        Value::String(s) => crate::util::repr_text(s),
        Value::Array(a) => format!("[{}]", a.iter().map(repr_value).collect::<Vec<_>>().join(", ")),
        Value::Object(o) => format!("{{{}}}", o.iter().map(|(k, v)| format!("{}: {}", crate::util::repr_text(k), repr_value(v)))
            .collect::<Vec<_>>().join(", ")),
        other => value_str(other),
    }
}

/// A JSON value in text: strings bare, lists and objects as `repr_value` writes them.
fn fstr(v: Option<&Value>) -> String {
    match v {
        Some(Value::Array(_)) | Some(Value::Object(_)) => repr_value(v.unwrap_or(&Value::Null)),
        _ => value_str_opt(v),
    }
}

// sdr-list --------------------------------------------------------------------------------------

pub fn format_sdr_list(devices: Option<&[Value]>, configured: &[Value], error: Option<&str>, auto: bool) -> String {
    if let Some(e) = error {
        return format!("cannot load the RTL-SDR driver: {e} (apt install rtl-sdr librtlsdr0)");
    }
    let mut lines = vec![];
    let ser = |d: &Value| d.get("serial").map(value_str).unwrap_or_else(|| "None".into());
    let devices = devices.unwrap_or(&[]);
    for d in devices {
        // a map by serial: the last configured entry wins
        let cfg = configured.iter().rev().find(|c| ser(c) == fstr(d.get("serial")));
        let role = match cfg {
            Some(c) => {
                let mut r = format!("role {}", value_str_opt(c.get("role")));
                if auto {
                    if let Some(a) = c.get("antenna").filter(|a| truthy(a)) {
                        r.push_str(&format!(" (automatic): connect {}", value_str(a)));
                    }
                }
                r
            }
            None if auto => "not used yet (roles are given out automatically)".into(),
            None => "not configured".into(),
        };
        let s = |k: &str| d.get(k).filter(|v| truthy(v)).map(value_str).unwrap_or_else(|| "?".into());
        let name = format!("{} {}", s("vendor"), s("product")).trim().to_string();
        lines.push(format!("#{}  serial {} {}  ->  {}", fstr(d.get("index")), ljust(&fstr(d.get("serial")), 10), name, role));
    }
    let present: Vec<String> = devices.iter().map(|d| fstr(d.get("serial"))).collect();
    for c in configured {
        let s = ser(c);
        if !present.contains(&s) {
            let hint = if PLACEHOLDER_SERIALS.contains(&s.as_str()) { " (placeholder: replace it with a serial above)" } else { "" };
            lines.push(format!("--  serial {} configured as role {} but not connected{}", ljust(&s, 10),
                               value_str_opt(c.get("role")), hint));
        }
    }
    if devices.is_empty() {
        lines.insert(0, format!("no RTL-SDR dongles found. {NO_DONGLES_HINT}"));
    }
    lines.join("\n")
}

/// What to check when no dongle is found. The appliance runs in a Proxmox container; a Mac sees
/// the dongles directly.
#[cfg(not(target_os = "macos"))]
pub const NO_DONGLES_HINT: &str = "On the Proxmox host run packaging/linux/proxmox-host-setup.sh <ctid> (USB \
                                   passthrough, udev rule and the DVB-T driver blacklist), then pct reboot <ctid>.";
#[cfg(target_os = "macos")]
pub const NO_DONGLES_HINT: &str = "Check that they are plugged in: System Information lists them under USB \
                                   (Realtek, RTL2838).";

pub fn cmd_sdr_list(settings: &Settings, lister: &Lister, out: &mut dyn Write) -> i32 {
    let (devices, err): (Option<Vec<Value>>, Option<String>) = match lister() {
        Ok(d) => (Some(d), None),
        Err(DriverError(e)) => (None, Some(e)),
    };
    let text = if auto_mode(settings.sdr()) {
        let saved = read_json(&settings.state_dir().join(ROLES_FILE)).filter(Value::is_object).unwrap_or_else(|| json!({}));
        let slots = role_configs(settings.sdr());
        let roles: Vec<String> = slots.iter().map(|c| value_str_opt(c.get("role"))).collect();
        let keys = RoleAssigner::new(&roles, &settings.state_dir(), railspotting_common::system_clock()).keys;
        let configured: Vec<Value> = keys.iter().zip(&slots).filter_map(|(k, cfg)| {
            let s = saved.get(k).filter(|v| truthy(v))?;
            let mut c = cfg.clone();
            c["serial"] = s.clone();
            Some(c)
        }).collect();
        format_sdr_list(devices.as_deref(), &configured, err.as_deref(), true)
    } else {
        format_sdr_list(devices.as_deref(), &settings.devices(), err.as_deref(), false)
    };
    let _ = writeln!(out, "{text}");
    if err.is_some() || devices.is_none_or(|d| d.is_empty()) { 1 } else { 0 }
}

// status ------------------------------------------------------------------------------------------

fn age(ts: Option<&Value>, now: f64) -> String {
    let Some(ts) = ts.filter(|v| truthy(v)).and_then(Value::as_f64) else { return "never".into() };
    let d = now - ts;
    if d < 120.0 {
        format!("{d:.0} s ago")
    } else if d < 7200.0 {
        format!("{:.0} min ago", d / 60.0)
    } else {
        format!("{:.1} h ago", d / 3600.0)
    }
}

pub fn describe_registration(st: &Value) -> String {
    let reg = st.get("registration");
    let lk = st.get("link").filter(|l| truthy(l));
    match (reg.and_then(Value::as_str), lk) {
        (Some("waiting"), Some(lk)) => {
            let left = (lk.get("expires_at").and_then(Value::as_f64).unwrap_or(0.0) - now()).max(0.0);
            format!("waiting for the owner: open {} and enter {} (expires in {:.0} min)",
                    value_str_opt(lk.get("verification_uri")), value_str_opt(lk.get("user_code")), left / 60.0)
        }
        (Some("denied"), _) => "linking denied by the owner; run `railspotting-receiver link` to start again".into(),
        (Some("linking"), _) => "linking (asking the headend for a code)".into(),
        (Some("checking"), _) => "checking the stored device ID with the headend".into(),
        _ => value_str_opt(reg),
    }
}

pub fn cmd_status(settings: &Settings, out: &mut dyn Write) -> i32 {
    let state = settings.state_dir();
    let ident = read_json(&state.join("identity.json")).filter(Value::is_object).unwrap_or_else(|| json!({}));
    let st = read_json(&state.join("status.json")).filter(Value::is_object);
    let now = now();
    let g = |v: &Value, k: &str| v.get(k).cloned().unwrap_or(Value::Null);
    let _ = writeln!(out, "device ID     {}  (MAC {} on {})",
                     ident.get("device_id").filter(|v| truthy(v)).map(value_str).unwrap_or_else(|| "(not linked yet)".into()),
                     fstr(ident.get("mac")), fstr(ident.get("interface")));
    let Some(st) = st else {
        let _ = writeln!(out, "service       no status yet (is railspotting-receiver running?)");
        return 1;
    };
    let ts = st.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
    let stale = st.get("stopped").is_some_and(truthy) || now - ts > 30.0;
    let _ = writeln!(out, "service       {}{}",
                     if stale { format!("NOT RUNNING (last status {})", age(st.get("ts"), now)) } else { "running".into() },
                     if st.get("simulate").is_some_and(truthy) { "  [simulation]" } else { "" });
    let _ = writeln!(out, "registration  {}", describe_registration(&st));
    let _ = writeln!(out, "server        {}  plan {}  clock {}", fstr(st.get("server_url")), fstr(st.get("config_version")),
                     if st.get("clock_synced").is_some_and(truthy) { "ok" } else { "NOT synced" });
    for s in st.get("sdrs").and_then(Value::as_array).into_iter().flatten() {
        let refs = match s.get("reference").filter(|r| truthy(r)) {
            Some(r) => format!("{} {:+.0} dB", fstr(r.get("name")), r.get("snr_db").and_then(Value::as_f64).unwrap_or(0.0)),
            None => "reference not in window".into(),
        };
        let dongle = match s.get("serial").filter(|v| truthy(v)) {
            Some(x) => format!("serial {}", value_str(x)),
            None => "no dongle assigned yet".into(),
        };
        let connected = s.get("connected").is_some_and(truthy);
        let state = if connected && s.get("corrupt").is_some_and(truthy) {
            "CORRUPT DATA (stuck; unplug it for 10 s and plug it back in)"
        } else if connected {
            "connected"
        } else if s.get("plugged_in") == Some(&json!(false)) {
            "UNPLUGGED"
        } else if s.get("plugged_in").is_some_and(truthy) {
            "plugged in, NOT STREAMING"
        } else {
            "NOT CONNECTED"
        };
        let port = s.get("usb_port").filter(|v| truthy(v)).map(|p| format!(", USB port {}", value_str(p))).unwrap_or_default();
        let _ = writeln!(out, "\n{} ({}{}, role {}): {} for {}", fstr(s.get("name")), dongle, port, fstr(s.get("role")), state,
                         age(s.get("since"), now).replace(" ago", ""));
        if s.get("assigned").and_then(Value::as_str) == Some("automatic") {
            let _ = writeln!(out, "  antenna: {} (role assigned automatically)", fstr(s.get("antenna")));
        }
        let _ = writeln!(out, "  center {} MHz, {:.2} MS/s, gain {} dB, window {}", fstr(s.get("center_mhz")),
                         s.get("rate").and_then(Value::as_f64).unwrap_or(0.0) / 1e6, fstr(s.get("gain")), fstr(s.get("window_mhz")));
        let _ = writeln!(out, "  clipping {:.2}%  blocks {}  overruns {}  short reads {}  stalls {}  {}",
                         s.get("clipping").and_then(Value::as_f64).unwrap_or(0.0) * 100.0, fstr(s.get("blocks")),
                         fstr(s.get("overruns")), fstr(s.get("short_reads")), fstr(s.get("stalls")), refs);
        if let Some(u) = s.get("uncovered").filter(|u| truthy(u)) {
            let _ = writeln!(out, "  channels outside the window: {}", repr_value(u));
        }
        for c in s.get("channels").and_then(Value::as_array).into_iter().flatten() {
            let floor = match c.get("floor_db").and_then(Value::as_f64) {
                Some(f) => float_repr(f),
                None => "learning".into(),
            };
            let label: String = crate::util::head(&fstr(c.get("label")), 28);
            let _ = writeln!(out, "    ch {} {:.4} {} floor {}{}", rjust(&fstr(c.get("ch")), 3),
                             c.get("freq").and_then(Value::as_f64).unwrap_or(0.0), ljust(&label, 28), rjust(&floor, 7),
                             if c.get("receiving").is_some_and(truthy) { "  RECEIVING" } else { "" });
        }
    }
    let sa = st.get("sdr_assign").cloned().unwrap_or_else(|| json!({}));
    let joined = |v: &Value| v.as_array().into_iter().flatten().map(value_str).collect::<Vec<_>>().join(", ");
    if sa.get("duplicates").is_some_and(truthy) {
        let _ = writeln!(out, "\nWARNING: dongles with serial {} can't be told apart and are not used; give each its own \
                               serial with rtl_eeprom", joined(&sa["duplicates"]));
    }
    if sa.get("spare").is_some_and(truthy) {
        let _ = writeln!(out, "\nnot needed: dongle {} (every role already has one)", joined(&sa["spare"]));
    }
    let sc = st.get("scanner").cloned().unwrap_or_else(|| json!({}));
    let conn = sc.get("connected").is_some_and(truthy);
    let mut line = format!("\nscanner       {}", if conn {
        format!("connected: {} {}", fstr(sc.get("model")), fstr(sc.get("firmware")))
    } else {
        "not connected".into()
    });
    if conn {
        line.push_str(&format!(", role {}, {}, cable {} on {}", fstr(st.get("scanner_role")), fstr(sc.get("state")),
                               fstr(sc.get("cable_serial")), fstr(sc.get("port"))));
    }
    let _ = writeln!(out, "{line}");
    if let Some(f) = st.get("focus").filter(|f| truthy(f)) {
        let _ = writeln!(out, "  focused on {:.4} MHz ({})", f.get("freq").and_then(Value::as_f64).unwrap_or(0.0), fstr(f.get("reason")));
    }
    let _ = writeln!(out, "\noutbox        {} queued, {} dropped", fstr(st.get("outbox_depth")), fstr(st.get("dropped_events")));
    if let Some(up) = st.get("uploaded").filter(|u| truthy(u)) {
        let n = |k: &str| up.get(k).map(value_str).unwrap_or_else(|| "0".into());
        let _ = writeln!(out, "uploaded      {} events accepted since start, {} with transcripts, {} with EOT data",
                         n("events"), n("transcripts"), n("telemetry"));
    }
    if let Some(Value::Object(api)) = st.get("api") {
        let mut eps: Vec<(&String, &Value)> = api.iter().collect();
        eps.sort_by(|a, b| a.0.cmp(b.0));
        for (ep, r) in eps {
            let what = r.get("status").filter(|v| truthy(v)).or(r.get("error"));
            let _ = writeln!(out, "  {} {} {}", ljust(ep, 20), fstr(what), age(r.get("ts"), now));
        }
    }
    let _ = writeln!(out, "\nlast events");
    let evs = st.get("last_events").and_then(Value::as_array).cloned().unwrap_or_default();
    for e in evs.iter().skip(evs.len().saturating_sub(5)) {
        let extra = if e.get("telemetry").is_some_and(truthy) { " +telemetry" }
                    else if e.get("transcript").is_some_and(truthy) { " +transcript" } else { "" };
        let _ = writeln!(out, "  {} {:.4} ch {} {} peak {}{}", ljust(&fstr(e.get("state")), 6),
                         e.get("freq").and_then(Value::as_f64).unwrap_or(0.0), fstr(e.get("ch")), fstr(e.get("kind")),
                         fstr(e.get("peak_rssi")), extra);
    }
    if let Some(e) = st.get("last_eot").filter(|v| truthy(v)) {
        let _ = writeln!(out, "last EOT      unit {}, {} psi, {}, {}", fstr(e.get("unit_id")), fstr(e.get("brake_pipe_psi")),
                         if e.get("motion").is_some_and(truthy) { "moving" } else { "stopped" }, age(e.get("ts"), now));
    }
    if let Some(t) = st.get("last_transcript").filter(|v| truthy(v)) {
        let _ = writeln!(out, "last words    \"{}\" ({}%) on {:.4}, {}", fstr(t.get("text")),
                         crate::util::round_int(t.get("confidence").and_then(Value::as_f64).unwrap_or(0.0) * 100.0),
                         t.get("freq").and_then(Value::as_f64).unwrap_or(0.0), age(t.get("ts"), now));
    }
    if let Some(r) = st.get("last_no_transcript").filter(|v| truthy(v)) {
        let _ = writeln!(out, "last reject   {}", crate::util::head(&json_dumps(r), 120));
    }
    let _ = g;
    0
}

// test-transcription, reset, link ----------------------------------------------------------------

pub fn cmd_test_transcription(settings: &Settings, wait_s: f64, out: &mut dyn Write) -> i32 {
    let state = settings.state_dir();
    let result = state.join(TT_RESULT);
    let start = now();
    if let Err(e) = std::fs::write(state.join(TT_TRIGGER), b"") {
        let _ = writeln!(out, "{e}");
        return 1;
    }
    let _ = writeln!(out, "asked the running service for a transcription test (about 20 s of recording)...");
    while now() - start < wait_s {
        if let Some(data) = read_json(&result) {
            if let (Some(ts), Some(line), Some(ok)) = (data.get("ts").and_then(Value::as_f64), data.get("line"), data.get("ok")) {
                if ts >= start - 1.0 {
                    let _ = writeln!(out, "{}", value_str(line));
                    return if truthy(ok) { 0 } else { 1 };
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    let _ = writeln!(out, "no result after 60 s. Is the service running? (systemctl status railspotting-receiver)");
    1
}

pub fn cmd_reset_identity(settings: &Settings, ask: &mut dyn FnMut(&str) -> String, out: &mut dyn Write) -> i32 {
    let _ = writeln!(out, "This deletes this receiver's device ID and key. It will have to be linked again (on the \
                           website, choose to replace the old receiver's hardware to keep its history).");
    if ask("Type RESET to continue: ").trim() != "RESET" {
        let _ = writeln!(out, "cancelled");
        return 1;
    }
    let _ = std::fs::remove_file(settings.state_dir().join("identity.json"));
    let path = settings.credentials_path();
    let mut creds = keys::load_credentials(&path).unwrap_or_default();
    creds.shift_remove("private_key");
    if let Err(e) = keys::save_credentials(&path, &creds) {
        let _ = writeln!(out, "{e}");
        return 1;
    }
    let _ = writeln!(out, "device ID and key deleted; restart the service to link it again");
    0
}

/// Start linking: through the running service if there is one, otherwise in the foreground.
pub fn cmd_link(settings: &Settings, wait_s: f64, out: &mut dyn Write) -> i32 {
    let state = settings.state_dir();
    let mut ident = read_json(&state.join("identity.json")).filter(Value::is_object).unwrap_or_else(|| json!({}));
    if let Some(id) = ident.get("device_id").filter(|v| truthy(v)) {
        let _ = writeln!(out, "already linked as {}; nothing to do", value_str(id));
        return 0;
    }
    let running = read_json(&state.join("status.json")).is_some_and(|st| {
        !st.get("stopped").is_some_and(truthy) && st.get("ts").and_then(Value::as_f64).is_some_and(|t| now() - t < 30.0)
    });
    if !running {
        if ident.get("link_denied").is_some_and(truthy) {
            ident["link_denied"] = json!(false);
            let _ = save_identity(&state, &ident);
        }
        let mut s = settings.clone();
        if let Some(o) = s.raw.as_object_mut() {
            o.insert("transcription".into(), json!(false));
            o.insert("web".into(), json!({"enabled": false}));
            if let Some(Value::Object(sdr)) = o.get_mut("sdr") {
                sdr.insert("devices".into(), json!([]));
            }
        }
        let agent = match Agent::new(s, AgentOptions::default()) {
            Ok(a) => a,
            Err(e) => {
                let _ = writeln!(out, "{e}");
                return 1;
            }
        };
        agent.registration_loop(true);
        return match agent.device_id() {
            Some(id) => {
                let _ = writeln!(out, "linked: {id}");
                0
            }
            None => {
                let _ = writeln!(out, "the owner denied the link");
                1
            }
        };
    }
    let _ = std::fs::write(state.join(LINK_TRIGGER), b"");
    let _ = writeln!(out, "asked the running service to start linking...");
    let start = now();
    while now() - start < wait_s {
        std::thread::sleep(std::time::Duration::from_secs(1));
        let Some(st) = read_json(&state.join("status.json")) else { continue };
        if st.get("ts").and_then(Value::as_f64).unwrap_or(0.0) > start
            && matches!(st.get("registration").and_then(Value::as_str), Some("waiting") | Some("approved")) {
            let _ = writeln!(out, "{}", describe_registration(&st));
            return 0;
        }
    }
    let _ = writeln!(out, "no link code yet; see `railspotting-receiver status` or the journal");
    1
}

// detector ---------------------------------------------------------------------------------------

/// A WAV path, or a clip ID such as 20261011-015604.540 from state/voice_clips.
pub fn read_clip(settings: &Settings, r: &str) -> Result<Vec<f32>, String> {
    let mut path = PathBuf::from(r);
    if !path.exists() {
        if r.contains('/') || r.ends_with(".wav") || r.contains(['*', '?', '[']) {
            return Err(format!("no such file: {r}"));
        }
        let dir = settings.state_dir().join("voice_clips");
        let mut hits: Vec<PathBuf> = std::fs::read_dir(&dir).into_iter().flatten().flatten().map(|e| e.path())
            .filter(|p| p.file_name().map(|n| n.to_string_lossy().to_string())
                .is_some_and(|n| n.starts_with(r) && n.ends_with(".wav")))
            .collect();
        hits.sort();
        path = hits.into_iter().next().ok_or_else(|| format!("no clip {r} (in {})", dir.display()))?;
    }
    let w = crate::wav::read(&path).map_err(|e| e.to_string())?;
    if w.rate != 16_000 || w.sample_width != 2 {
        return Err(format!("{}: need 16 kHz 16-bit WAV like the saved voice clips", path.display()));
    }
    Ok(w.samples())
}

fn mtime(p: &Path) -> f64 {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// Run the detector matching on the saved clips of the last `hours` and, with `send`, queue
/// the v1.5 detector events for the running service to upload.
///
/// Each clip is paired with its original radio event from state/history.db (by end time and
/// length). The original was already sent without `detector`, and the headend ignores a second
/// closed event with the same ID, so the replayed event gets a new, stable ID (the same on
/// every run) and `detector.timestamp` = the original start, for the headend's historical load.
/// Clips are what's on disk: state/detector_clips and the newest 50 in state/voice_clips.
pub fn cmd_detector_replay(settings: &Settings, hours: f64, send: bool, at: Option<f64>, out: &mut dyn Write) -> i32 {
    let now = at.unwrap_or_else(now);
    let state = settings.state_dir();
    let since = now - hours * 3600.0;
    let mut clips: Vec<(String, PathBuf)> = vec![];
    for folder in [state.join("detector_clips"), state.join("voice_clips")] {
        let mut files: Vec<PathBuf> = std::fs::read_dir(&folder).into_iter().flatten().flatten().map(|e| e.path())
            .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().ends_with("_voice.wav"))).collect();
        files.sort();
        for f in files {
            let name = f.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            if mtime(&f) >= since && !clips.iter().any(|(n, _)| *n == name) {
                clips.push((name, f));
            }
        }
    }
    let rows = (|| -> rusqlite::Result<Vec<Value>> {
        let db = rusqlite::Connection::open_with_flags(state.join("history.db"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let mut q = db.prepare("select payload from events where kind = 'voice' and start_ts >= ?")?;
        let rows = q.query_map([since - 600.0], |r| r.get::<_, String>(0))?
            .filter_map(|r| r.ok()).filter_map(|t| serde_json::from_str(&t).ok()).collect();
        Ok(rows)
    })();
    let rows = match rows {
        Ok(r) => r,
        Err(e) => {
            let _ = writeln!(out, "cannot read the event history: {e}");
            return 1;
        }
    };
    let f = |e: &Value, k: &str| e.get(k).and_then(Value::as_f64).unwrap_or(0.0);
    let events: Vec<Value> = rows.into_iter().filter(|e| e.get("state").and_then(Value::as_str) == Some("closed")
        && e.get("end_ts").is_some_and(truthy)).collect();
    let mut pairs: Vec<(Value, PathBuf)> = vec![];
    for (_, p) in &clips {
        let Ok(w) = crate::wav::read(p) else { continue };
        let clip_s = w.n_frames() as f64 / w.rate.max(1) as f64;
        let saved = mtime(p);
        let best = events.iter().filter(|e| {
            let d = saved - f(e, "end_ts");
            (-1.0..=180.0).contains(&d) && ((f(e, "end_ts") - f(e, "start_ts")) + CLIP_LEAD_S - clip_s).abs() <= 0.6
        }).min_by(|a, b| (saved - f(a, "end_ts")).total_cmp(&(saved - f(b, "end_ts"))));
        if let Some(e) = best {
            pairs.push((e.clone(), p.clone()));
        }
    }
    pairs.sort_by(|a, b| f(&a.0, "start_ts").total_cmp(&f(&b.0, "start_ts")));
    let _ = writeln!(out, "{} clips from the last {} h; {} matched to their events", clips.len(), fmt_g(hours), pairs.len());

    let tr = Transcriber::new(&settings.models_dir(), None, 50, None, Some(&state.join("detector_templates")), true);
    if tr.template_list().is_none_or(|l| l.is_empty()) {
        let _ = writeln!(out, "no detector templates");
        return 1;
    }
    let cached = load_cached_config(&state);
    tr.set_known_detectors(cached.get("detectors").and_then(Value::as_array).cloned());
    tr.set_nearby_detectors(cached.get("nearby_detectors").and_then(Value::as_array).cloned());
    tr.set_profile_config(&cached);
    let mut queued: Vec<Value> = vec![];
    for (ev, p) in &pairs {
        let Ok(w) = crate::wav::read(p) else { continue };
        let audio = w.samples();
        let eid = value_str_opt(ev.get("event_id"));
        let new_id = uuid::Uuid::new_v5(&REPLAY_NAMESPACE, eid.as_bytes()).to_string();
        let meta = json!({"event_id": new_id, "start_ts": ev["start_ts"], "end_ts": ev["end_ts"], "timestamp": ev["start_ts"]});
        let mut res = match tr.transcribe(&audio, Some(&meta), false) {
            Ok(Some(r)) if r.contains_key("_detector") => r,
            _ => continue,
        };
        let det = res.shift_remove("_detector").unwrap_or(Value::Null);
        let mut payload = Map::new();
        for k in ["state", "start_ts", "end_ts", "freq", "ch", "kind", "peak_rssi", "avg_rssi", "samples"] {
            if let Some(v) = ev.get(k) {
                payload.insert(k.into(), v.clone());
            }
        }
        payload.insert("event_id".into(), json!(new_id));
        payload.insert("transcript".into(), Value::Object(res));
        payload.insert("detector".into(), det.clone());
        queued.push(Value::Object(payload));
        let _ = writeln!(out, "  {} UTC  {} {} {} {:.1} s{}", crate::util::utc_stamp(f(ev, "start_ts"), "%m-%d %H:%M:%S"),
                         fstr(det.get("detector_id")), ljust(&fstr(det.get("result")), 17), ljust(&fstr(det.get("method")), 17),
                         f(&det, "length_s"), if det.get("repeat_of").is_some_and(truthy) { "  repeat" } else { "" });
    }
    if queued.is_empty() {
        let _ = writeln!(out, "no detector messages in these clips");
        return 0;
    }
    if !send {
        let _ = writeln!(out, "{} detector events; add --send to queue them for the headend", queued.len());
        let _ = writeln!(out, "{}", json_dumps(&queued[0]));
        return 0;
    }
    let outbox = match Outbox::open(state.join("outbox.db")) {
        Ok(o) => o,
        Err(e) => {
            let _ = writeln!(out, "cannot open the outbox: {e:?}");
            return 1;
        }
    };
    for p in &queued {
        let _ = outbox.add_event(p);
    }
    outbox.close();
    let _ = writeln!(out, "queued {} detector events; the running service uploads them (watch: journalctl -u \
                           railspotting-receiver -f)", queued.len());
    0
}

fn opt_list(v: &Option<Vec<Option<f64>>>) -> String {
    crate::util::list_text_opt(v.as_deref())
}

/// Announcements kept per detector in state/detector_candidates, newest last, with what was
/// heard: the material for a new template.
pub fn cmd_detector_candidates(settings: &Settings, out: &mut dyn Write) -> i32 {
    let root = settings.state_dir().join("detector_candidates");
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&root).into_iter().flatten().flatten().map(|e| e.path())
        .filter(|p| p.is_dir()).collect();
    dirs.sort();
    if dirs.is_empty() {
        let _ = writeln!(out, "no candidate announcements yet (calls on watched channels are kept in {})", root.display());
        return 0;
    }
    for dir in dirs {
        let mut metas: Vec<PathBuf> = std::fs::read_dir(&dir).into_iter().flatten().flatten().map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json")).collect();
        metas.sort();
        let id = dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let _ = writeln!(out, "{id}: {} recordings ({})", metas.len(), dir.display());
        for m in metas {
            let Ok(meta) = std::fs::read_to_string(&m).map_err(|e| e.to_string())
                .and_then(|t| serde_json::from_str::<Value>(&t).map_err(|e| e.to_string())) else { continue };
            let clip = m.file_name().map(|n| n.to_string_lossy().trim_end_matches(".json").to_string()).unwrap_or_default();
            let heard = meta.get("milepost_heard").and_then(Value::as_f64).map_or("-".into(), fmt_g);
            let axles = meta.pointer("/parsed/axles").map_or("-".into(), |v| v.to_string());
            let _ = writeln!(out, "  {clip}  milepost heard {heard} ({})  axles {axles}  \"{}\"",
                             meta.get("match").and_then(Value::as_str).unwrap_or("-"),
                             meta.get("text").and_then(Value::as_str).unwrap_or(""));
        }
    }
    0
}

pub fn cmd_detector(settings: &Settings, action: &DetectorCmd, out: &mut dyn Write) -> i32 {
    let mut templates = DetectorTemplates::new(settings.state_dir().join("detector_templates"));
    let clip_refs = match action {
        DetectorCmd::List => {
            let items = templates.list();
            if items.is_empty() {
                let _ = writeln!(out, "no detector templates");
            }
            for t in items {
                let split = match t.get("split_s").filter(|v| truthy(v)) {
                    Some(s) => format!("opening {} s", value_str(s)),
                    None => "whole only".into(),
                };
                let _ = writeln!(out, "{} {} s ({})  from {} recordings  \"{}\"  {}", ljust(&fstr(t.get("name")), 20),
                                 rjust(&fstr(t.get("seconds")), 5), split, fstr(t.get("recordings")), fstr(t.get("text")),
                                 json_dumps(t.get("parsed").unwrap_or(&Value::Null)));
            }
            return 0;
        }
        DetectorCmd::Replay { hours, send } => return cmd_detector_replay(settings, *hours, *send, None, out),
        DetectorCmd::Candidates => return cmd_detector_candidates(settings, out),
        DetectorCmd::Remove { name } => {
            let ok = templates.remove(name);
            let _ = writeln!(out, "{}", if ok { "removed".to_string() } else { format!("no template {name}") });
            return if ok { 0 } else { 1 };
        }
        DetectorCmd::Test { clips } => clips,
        DetectorCmd::Add { clips, .. } => clips,
    };
    let mut clips = vec![];
    for c in clip_refs {
        match read_clip(settings, c) {
            Ok(a) => clips.push(a),
            Err(e) => {
                let _ = writeln!(out, "{e}");
                return 1;
            }
        }
    }
    match action {
        DetectorCmd::Test { clips: refs } => {
            let items = templates.list();
            for (r, audio) in refs.iter().zip(&clips) {
                let call = features(audio);
                let mut res: Vec<(String, Assessment)> = vec![];
                for t in &templates.items {
                    let split = DetectorTemplates::split_frames(&t.info);
                    let mut a = assess(t.features.view(), call.view(), split, DetectorTemplates::opening_end_frames(&t.info));
                    if a.kind.is_some() {
                        check_length_and_rhythm(t, &mut a, audio, split);
                    }
                    res.push((t.name().to_string(), a));
                }
                let detail = res.iter().map(|(n, a)| {
                    let mut s = format!("{n} opening {} ending {}", opt_list(&a.opening), opt_list(&a.ending));
                    if let Some(r) = a.rhythm {
                        s.push_str(&format!(" rhythm {}", float_repr(r)));
                    }
                    if let Some(l) = a.length_s {
                        s.push_str(&format!(" length {} s", float_repr(l)));
                    }
                    if let Some(m) = a.milepost.as_ref().filter(|m| !m.is_empty()) {
                        s.push_str(&format!(" milepost {m}"));
                    }
                    s
                }).collect::<Vec<_>>().join("  ");
                // sorted ((all_clear, score), name, kind): the last is the best
                let best = res.iter().filter(|(_, a)| a.kind.is_some()).max_by(|(n1, a1), (n2, a2)| {
                    (a1.is("all_clear"), a1.score).partial_cmp(&(a2.is("all_clear"), a2.score))
                        .unwrap_or(std::cmp::Ordering::Equal).then(n1.cmp(n2))
                });
                let verdict = match best {
                    Some((n, a)) => {
                        let how = match a.method.as_deref() {
                            None | Some("sound") => String::new(),
                            Some(m) => format!(", by {m}"),
                        };
                        format!("{n} ({}{how})", a.kind.clone().unwrap_or_default().replace('_', "-"))
                    }
                    None => "no match".into(),
                };
                let _ = writeln!(out, "{r}: {detail}  -> {verdict}");
            }
            if items.is_empty() { 1 } else { 0 }
        }
        DetectorCmd::Add { name, text, split, opening_end, clips: refs } => {
            if clips.len() < 3 {
                let _ = writeln!(out, "give at least 3 recordings of the same announcement; more cancel more noise");
                return 1;
            }
            let shortest = clips.iter().map(Vec::len).min().unwrap_or(0) as f64 / 16_000.0;
            if let Some(s) = split {
                if !(1.0 <= *s && *s <= shortest - 1.0) {
                    let _ = writeln!(out, "not added: split must be between 1 s and {:.1} s (the shortest clip is {:.1} s)",
                                     shortest - 1.0, shortest);
                    return 1;
                }
            }
            if let Some(oe) = opening_end {
                if split.is_none_or(|s| !(0.5 <= *oe && *oe <= s - 0.3)) {
                    let _ = writeln!(out, "not added: --opening-end needs --split, and must be between 0.5 s and the split");
                    return 1;
                }
            }
            let checks = match templates.scores(&clips, *split, *opening_end) {
                Ok(c) => c,
                Err(e) => {
                    let _ = writeln!(out, "not added: {e}");
                    return 1;
                }
            };
            for (r, a) in refs.iter().zip(&checks) {
                let _ = writeln!(out, "  {r}: opening {} ending {} milepost {} -> {} (against a template made from the others)",
                                 opt_list(&a.opening), opt_list(&a.ending), a.milepost.as_deref().unwrap_or("None"),
                                 a.kind.as_deref().unwrap_or("None"));
            }
            let bad: Vec<&String> = refs.iter().zip(&checks)
                .filter(|(_, a)| !a.is("all_clear") || a.milepost.as_deref() == Some("other")).map(|(r, _)| r).collect();
            if !bad.is_empty() {
                let _ = writeln!(out, "not added: {} don't match the others in every piece; leave them out or check that \
                                       every clip is the same all-clear announcement",
                                 bad.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "));
                return 1;
            }
            let parsed = railspotting_detect::text::parse(text, true);
            let info = match templates.add(name, text, &clips, parsed.clone(), *split, *opening_end) {
                Ok(i) => i,
                Err(e) => {
                    let _ = writeln!(out, "not added: {e}");
                    return 1;
                }
            };
            let g = |k: &str| fstr(info.get(k));
            let split_text = if info.get("opening_end_s").is_some_and(truthy) {
                format!(", wording \"{}\" to {} s (any milepost), \"{}\" from {} s", g("wording_text"), g("opening_end_s"),
                        g("ending_text"), g("split_s"))
            } else if info.get("split_s").is_some_and(truthy) {
                format!(", opening \"{}\" ends at {} s", g("opening_text"), g("split_s"))
            } else {
                " (no --split: only the whole announcement is recognized)".into()
            };
            let _ = writeln!(out, "added {}: {} s from {} recordings, \"{}\" {}{}", g("name"), g("seconds"), g("recordings"),
                             g("text"), json_dumps(&parsed.unwrap_or(Value::Null)), split_text);
            let _ = writeln!(out, "the running service picks it up with the next voice call");
            0
        }
        _ => 0,
    }
}

// main -------------------------------------------------------------------------------------------

fn stdin_ask(prompt: &str) -> String {
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    let _ = std::io::stdin().read_line(&mut s);
    s
}

/// The rekey option: a new key, and the device ID forgotten.
pub fn cmd_rekey(settings: &Settings, out: &mut dyn Write) -> i32 {
    if let Err(e) = keys::replace_key(&settings.credentials_path()) {
        let _ = writeln!(out, "{e}");
        return 1;
    }
    let state = settings.state_dir();
    let ident_path = state.join("identity.json");
    if ident_path.exists() {
        let Some(mut ident) = read_json(&ident_path).filter(Value::is_object) else {
            let _ = writeln!(out, "{}: not valid JSON", ident_path.display());
            return 1;
        };
        let old = ident.get("device_id").cloned().unwrap_or(Value::Null);
        ident["device_id"] = Value::Null;
        ident["link_denied"] = json!(false);
        let _ = save_identity(&state, &ident);
        if truthy(&old) {
            let _ = writeln!(out, "forgot device ID {}; when linking, choose to replace its hardware to keep its settings \
                                   and history", value_str(&old));
        }
    }
    let _ = writeln!(out, "new key created; restart the service (systemctl restart railspotting-receiver) to link it");
    0
}

pub fn main_with(argv: Vec<String>, out: &mut dyn Write) -> i32 {
    let args = match Args::try_parse_from(argv) {
        Ok(a) => a,
        Err(e) => {
            let _ = e.print();
            return if e.use_stderr() { 2 } else { 0 };
        }
    };
    // ITC commands need neither receiver.json nor the receiver's log, which goes to stdout:
    // theirs is JSON for other programs (progress goes to stderr)
    if let Some(Cmd::Itc { action }) = &args.cmd {
        return crate::itc::cmd_itc(action, out);
    }
    let mut config = PathBuf::from(&args.config);
    if args.config == "receiver.json" && !config.exists() && Path::new(INSTALLED_CONFIG).exists() {
        config = PathBuf::from(INSTALLED_CONFIG);
    }
    // warnings are printed even before the configured log level is known
    crate::logging::setup("WARNING");
    let settings = match Settings::load(&config) {
        Ok(s) => s,
        Err(e) => {
            let _ = writeln!(out, "{e}");
            return 1;
        }
    };
    crate::logging::setup(settings.str("log_level").unwrap_or("INFO"));
    if args.reset_identity {
        return cmd_reset_identity(&settings, &mut stdin_ask, out);
    }
    if args.rekey {
        return cmd_rekey(&settings, out);
    }
    match &args.cmd {
        Some(Cmd::SdrList) => cmd_sdr_list(&settings, &(Arc::new(crate::rtl::list_devices) as Lister), out),
        Some(Cmd::Status) => cmd_status(&settings, out),
        Some(Cmd::TestTranscription) => cmd_test_transcription(&settings, 60.0, out),
        Some(Cmd::Detector { action }) => cmd_detector(&settings, action, out),
        Some(Cmd::Itc { action }) => crate::itc::cmd_itc(action, out),
        Some(Cmd::Link) => cmd_link(&settings, 30.0, out),
        Some(Cmd::Run { simulate }) => run(settings, AgentOptions { simulate: *simulate, ..Default::default() }),
        None => {
            let _ = writeln!(out, "{}", <Args as clap::CommandFactory>::command().render_help());
            2
        }
    }
}

use std::sync::Arc;

/// The token for the settings endpoints when `web.settings_api` is on. The status page has no
/// login, so they are off on Linux unless asked for; the token goes in `state/web_token`
/// (readable by the service user only) for a local tool to send.
fn settings_api_token(settings: &Settings) -> Option<String> {
    if settings.get("web").get("settings_api") != Some(&Value::Bool(true)) {
        return None;
    }
    let token = crate::handle::new_token();
    let path = settings.state_dir().join("web_token");
    let written = std::fs::create_dir_all(settings.state_dir()).and_then(|()| {
        let mut f = std::fs::OpenOptions::new();
        f.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut f, 0o600);
        f.open(&path)?.write_all(token.as_bytes())
    });
    match written {
        Ok(()) => {
            log::info!("settings endpoints on (from 127.0.0.1 only); token in {}", path.display());
            Some(token)
        }
        Err(e) => {
            log::error!("settings endpoints off: {}: {e}", path.display());
            None
        }
    }
}

fn run(settings: Settings, opts: AgentOptions) -> i32 {
    let token = settings_api_token(&settings);
    match crate::handle::Engine::start(settings, crate::handle::EngineOptions { agent: opts, token }) {
        Ok(engine) => {
            wait_for_term();
            log::info!("stopping");
            engine.stop();
            0
        }
        Err(e) => {
            log::error!("{e}");
            1
        }
    }
}

/// Until SIGTERM or SIGINT.
fn wait_for_term() {
    let term = Arc::new(std::sync::atomic::AtomicBool::new(false));
    for sig in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
        let _ = signal_hook::flag::register(sig, term.clone());
    }
    while !term.load(std::sync::atomic::Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}
