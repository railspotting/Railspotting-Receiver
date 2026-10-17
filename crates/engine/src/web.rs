//! Local status page: a read-only HTTP server on the LAN, with no login.
//!
//! GET /             the page (web/dashboard.html)
//! GET /api/status   live status, as `status` shows it, without the scanner cable's serial number
//! GET /api/events   event history, newest first (?limit=, ?before=, ?kind=)
//! GET /api/signal   signal readings (?hours=, up to 7 days)
//! GET /link-qr.svg  QR code of the link address while the receiver waits for its owner (v1.4)
//! GET /api/recordings  saved recordings, newest first (?limit=, ?source= one or several, comma separated)
//! GET /recordings/<folder>/<name>_voice.wav  one recording (?download=1 to save it)
//!
//! Settings, only when the host has a token (the desktop app, or `web.settings_api` on Linux),
//! and only from 127.0.0.1 with the token in the `X-RailSpotting-Token` header:
//! GET  /api/settings  the settings the dashboard can change
//! POST /api/settings  change them (a JSON object; see `handle::validate_settings`)
//! POST /api/link      start linking again
//! POST /api/profiles/test  read a saved recording with a voice profile ({"recording": "/recordings/...",
//!                          "profile": id}); takes up to a minute and changes nothing
//!
//! GET /api/profiles is open like the rest of the page: the voice profiles, the channels with the
//! profile each uses and the detectors heard on it. Profiles and their channels are set on the
//! headend and arrive in /config; the receiver doesn't change them.

use crate::agent::Agent;
use crate::util::json_dumps;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::thread::JoinHandle;

pub const MAX_HOURS: f64 = 24.0 * 7.0;
pub const PAGE: &[u8] = include_bytes!("../web/dashboard.html");
pub const TOKEN_HEADER: &str = "X-RailSpotting-Token";
/// Settings requests are small; anything bigger is refused unread.
pub const MAX_BODY: usize = 64 * 1024;

/// What the page serves. The agent serves itself (read only); the app's engine serves whichever
/// agent is current, so the page survives a restart after a settings change.
pub trait WebHost: Send + Sync {
    fn agent(&self) -> Option<Arc<Agent>>;
    /// The per-run token. None turns the settings endpoints off.
    fn token(&self) -> Option<String> {
        None
    }
    fn settings(&self) -> Result<Value, String> {
        Err("settings are not offered here".into())
    }
    fn apply_settings(&self, _patch: Value) -> Result<Value, String> {
        Err("settings are not offered here".into())
    }
    fn relink(&self) -> Result<Value, String> {
        Err("linking is not offered here".into())
    }
}

impl WebHost for Weak<Agent> {
    fn agent(&self) -> Option<Arc<Agent>> {
        self.upgrade()
    }
}

/// The status for the LAN page. Dongle serials stay, so the owner can tell the dongles apart
/// and match antennas; the scanner cable's serial isn't needed there.
pub fn public_status(status: &Value) -> Value {
    let mut out = status.clone();
    if let Some(Value::Object(sc)) = out.get_mut("scanner") {
        sc.shift_remove("cable_serial");
    }
    out
}

/// Recording folders in the state dir, most specific first: a recording copied into several
/// is listed once, under the first. Subfolders hold one detector or one watched channel each.
pub const RECORDING_SOURCES: [(&str, &str, bool); 4] = [
    ("detector_candidates", "candidate", true),
    ("detector_clips", "detector", false),
    ("watch_clips", "watched", true),
    ("voice_clips", "voice", false),
];
pub const RECORDINGS_LIMIT: usize = 500;

/// A recording's file name as the receiver writes it: "20261012-195805.363_voice.wav".
fn recording_name_ok(name: &str) -> bool {
    let Some(stem) = name.strip_suffix("_voice.wav") else { return false };
    let b = stem.as_bytes();
    b.len() == 19 && b[8] == b'-' && b[15] == b'.'
        && b.iter().enumerate().all(|(i, c)| i == 8 || i == 15 || c.is_ascii_digit())
}

/// A detector or channel subfolder name: "csx-68.1", "161.1000".
fn subfolder_ok(name: &str) -> bool {
    !name.is_empty() && name.len() <= 40 && !name.starts_with('.')
        && name.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'.')
}

/// The file for a /recordings/ path, if it is a recording in one of the recording folders. Only
/// these names are served, so nothing else in the state dir (identity, keys) can be fetched.
pub fn recording_path(state: &Path, rel: &str) -> Option<PathBuf> {
    let parts: Vec<&str> = rel.split('/').collect();
    let (source, sub, name) = match parts.as_slice() {
        [source, name] => (*source, None, *name),
        [source, sub, name] => (*source, Some(*sub), *name),
        _ => return None,
    };
    let (_, _, nested) = RECORDING_SOURCES.iter().find(|(folder, _, _)| *folder == source)?;
    if *nested != sub.is_some() || !sub.is_none_or(subfolder_ok) || !recording_name_ok(name) {
        return None;
    }
    let mut p = state.join(source);
    if let Some(sub) = sub {
        p = p.join(sub);
    }
    let p = p.join(name);
    p.is_file().then_some(p)
}

fn read_meta(wav: &Path) -> Value {
    let json = wav.with_file_name(wav.file_name().and_then(|n| n.to_str()).unwrap_or("").replace(".wav", ".json"));
    std::fs::read_to_string(json).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null)
}

/// Saved recordings, newest first, with what the receiver made of each.
pub fn recordings(state: &Path, source: Option<&str>, limit: usize) -> Vec<Value> {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<(f64, Value)> = vec![];
    for (folder, kind, nested) in RECORDING_SOURCES {
        let dirs: Vec<(PathBuf, Option<String>)> = if nested {
            std::fs::read_dir(state.join(folder)).into_iter().flatten().flatten()
                .filter(|e| e.path().is_dir())
                .filter_map(|e| e.file_name().to_str().filter(|n| subfolder_ok(n)).map(|n| (e.path(), Some(n.to_string()))))
                .collect()
        } else {
            vec![(state.join(folder), None)]
        };
        for (dir, sub) in dirs {
            for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if !recording_name_ok(&name) || !seen.insert(name.clone())
                    || source.is_some_and(|s| !s.split(',').any(|one| one == kind)) {
                    continue;
                }
                let md = e.metadata().ok();
                let ts = md.as_ref().and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0.0, |d| d.as_secs_f64());
                // 16 kHz 16-bit mono after a 44-byte header
                let seconds = md.map_or(0.0, |m| (m.len().saturating_sub(44)) as f64 / 32_000.0);
                let meta = read_meta(&e.path());
                let tr = meta.get("transcript").filter(|t| t.is_object());
                let text = meta.get("reference_text").or_else(|| meta.get("text"))
                    .or_else(|| tr.and_then(|t| t.get("text"))).cloned().unwrap_or(Value::Null);
                let detector = meta.get("detector_id").or_else(|| tr.and_then(|t| t.pointer("/_detector/detector_id")))
                    .cloned().unwrap_or(Value::Null);
                let freq = meta.get("freq").or_else(|| meta.pointer("/event/freq")).cloned()
                    .or_else(|| sub.as_ref().filter(|_| folder == "watch_clips").and_then(|s| s.parse::<f64>().ok()).map(Value::from))
                    .unwrap_or(Value::Null);
                let path = match &sub {
                    Some(s) => format!("{folder}/{s}/{name}"),
                    None => format!("{folder}/{name}"),
                };
                out.push((ts, serde_json::json!({
                    "name": name, "source": kind, "group": sub, "ts": (ts * 10.0).round() / 10.0,
                    "seconds": (seconds * 10.0).round() / 10.0, "freq": freq, "detector": detector, "text": text,
                    "url": format!("/recordings/{path}"),
                })));
            }
        }
    }
    out.sort_by(|a, b| b.0.total_cmp(&a.0));
    out.into_iter().take(limit).map(|(_, v)| v).collect()
}

/// The link address as a QR code (error correction M, 6 px modules, 2 module border).
pub fn link_qr_svg(uri: &str) -> Result<Vec<u8>, String> {
    let code = qrcode::QrCode::with_error_correction_level(uri.as_bytes(), qrcode::EcLevel::M).map_err(|e| e.to_string())?;
    let n = code.width();
    let (scale, border) = (6usize, 2usize);
    let size = (n + 2 * border) * scale;
    let colors = code.to_colors();
    let mut path = String::new();
    for y in 0..n {
        let mut x = 0;
        while x < n {
            if colors[y * n + x] == qrcode::Color::Dark {
                let start = x;
                while x < n && colors[y * n + x] == qrcode::Color::Dark {
                    x += 1;
                }
                path.push_str(&format!("M{} {}h{}v{}h-{}z", (start + border) * scale, (y + border) * scale,
                                       (x - start) * scale, scale, (x - start) * scale));
            } else {
                x += 1;
            }
        }
    }
    Ok(format!("<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{size}\" height=\"{size}\" class=\"segno\">\
                <rect width=\"{size}\" height=\"{size}\" fill=\"#fff\"/><path fill=\"#000\" d=\"{path}\"/></svg>\n").into_bytes())
}

fn query_args(q: &str) -> Vec<(String, String)> {
    q.split(['&', ';']).filter(|p| !p.is_empty()).filter_map(|p| {
        let (k, v) = p.split_once('=')?; // parse_qs drops a name without "="
        let dec = |s: &str| {
            let s = s.replace('+', " ");
            let b = s.as_bytes();
            let mut out = vec![];
            let mut i = 0;
            while i < b.len() {
                if b[i] == b'%' && i + 2 < b.len() {
                    let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("");
                    if let Ok(x) = u8::from_str_radix(hex, 16) {
                        out.push(x);
                        i += 3;
                        continue;
                    }
                }
                out.push(b[i]);
                i += 1;
            }
            String::from_utf8_lossy(&out).into_owned()
        };
        // parse_qs drops blank values too
        let v = dec(v);
        if v.is_empty() { None } else { Some((dec(k), v)) }
    }).collect()
}

pub struct WebUI {
    host: Arc<dyn WebHost>,
    pub bind: String,
    pub port: u16,
    server: Option<Arc<tiny_http::Server>>,
    thread: Option<JoinHandle<()>>,
}

type Reply = (u16, &'static str, Vec<u8>);

impl WebUI {
    pub fn new(agent: Weak<Agent>, bind: &str, port: u16) -> WebUI {
        WebUI::with_host(Arc::new(agent), bind, port)
    }

    pub fn with_host(host: Arc<dyn WebHost>, bind: &str, port: u16) -> WebUI {
        WebUI { host, bind: bind.into(), port, server: None, thread: None }
    }

    pub fn start(&mut self) -> bool {
        let server = match tiny_http::Server::http((self.bind.as_str(), self.port)) {
            Ok(s) => Arc::new(s),
            Err(e) => {
                log::warn!("status page not started on {}:{}: {}", self.bind, self.port, e);
                return false;
            }
        };
        if let Some(a) = server.server_addr().to_ip() {
            self.port = a.port();
        }
        let (srv, host) = (server.clone(), self.host.clone());
        self.thread = std::thread::Builder::new().name("web".into()).spawn(move || {
            // one thread per request, as ThreadingHTTPServer: a slow client can't hold the page
            for req in srv.incoming_requests() {
                let host = host.clone();
                let _ = std::thread::Builder::new().name("web-req".into()).spawn(move || handle(host.as_ref(), req));
            }
        }).ok();
        self.server = Some(server);
        log::info!("status page on http://{}:{}/", self.bind, self.port);
        true
    }

    pub fn running(&self) -> bool {
        self.server.is_some()
    }

    pub fn stop(&mut self) {
        if let Some(s) = self.server.take() {
            s.unblock();
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for WebUI {
    fn drop(&mut self) {
        if let Some(s) = self.server.take() {
            s.unblock();
        }
    }
}

fn json_reply(v: &Value) -> Reply {
    (200, "application/json", json_dumps(v).into_bytes())
}

/// The response for a GET of `path` with its query.
pub fn route(agent: &Agent, path: &str, query: &str) -> Reply {
    let q = query_args(query);
    let get = |name: &str| q.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
    let int = |name: &str, d: i64| get(name).and_then(|v| v.trim().parse::<i64>().ok()).unwrap_or(d);
    let float = |name: &str| get(name).and_then(|v| v.trim().parse::<f64>().ok());
    match path {
        "/" | "/index.html" => (200, "text/html; charset=utf-8", PAGE.to_vec()),
        "/api/status" => json_reply(&public_status(&agent.status())),
        "/api/events" => {
            let rows = agent.history.events(int("limit", 100), float("before"), get("kind").filter(|k| !k.is_empty()))
                .unwrap_or_default();
            json_reply(&Value::Array(rows))
        }
        "/link-qr.svg" => match agent.link_info() {
            None => (404, "text/plain; charset=utf-8", b"not waiting for a link\n".to_vec()),
            Some(lk) => {
                let uri = lk.get("verification_uri_complete").and_then(Value::as_str).filter(|s| !s.is_empty())
                    .or_else(|| lk.get("verification_uri").and_then(Value::as_str)).unwrap_or("");
                match link_qr_svg(uri) {
                    Ok(svg) => (200, "image/svg+xml", svg),
                    Err(e) => {
                        log::error!("status page request {path} failed: {e}");
                        (500, "text/plain; charset=utf-8", b"internal error\n".to_vec())
                    }
                }
            }
        },
        "/api/profiles" => match agent.transcriber() {
            Some(tr) => result_reply(tr.profiles()),
            None => result_reply(Err("speech is off on this receiver".into())),
        },
        "/api/recordings" => {
            let limit = int("limit", 100).clamp(1, RECORDINGS_LIMIT as i64) as usize;
            json_reply(&Value::Array(recordings(&agent.state_dir, get("source"), limit)))
        }
        p if p.starts_with("/recordings/") => match recording_path(&agent.state_dir, &p["/recordings/".len()..])
            .and_then(|f| std::fs::read(f).ok()) {
            Some(bytes) => (200, "audio/wav", bytes),
            None => (404, "text/plain; charset=utf-8", b"no such recording\n".to_vec()),
        },
        "/api/signal" => {
            let hours = float("hours").unwrap_or(24.0).clamp(0.1, MAX_HOURS);
            let rows = agent.history.signal((agent.clock)() - hours * 3600.0).unwrap_or_default();
            json_reply(&Value::Array(rows))
        }
        _ => (404, "text/plain; charset=utf-8", b"not found\n".to_vec()),
    }
}

/// Whether a settings request may go ahead: the host offers settings, the request comes from
/// this machine, and it carries the token.
pub fn settings_allowed(token: Option<&str>, remote: Option<std::net::SocketAddr>, given: Option<&str>) -> Result<(), Reply> {
    let forbidden = |m: &str| Err((403, "text/plain; charset=utf-8", format!("{m}\n").into_bytes()));
    let Some(token) = token else { return forbidden("settings are not offered here") };
    if !remote.is_some_and(|r| r.ip().is_loopback()) {
        return forbidden("settings can only be changed on this machine");
    }
    match given {
        Some(g) if same_token(g, token) => Ok(()),
        _ => forbidden("missing or wrong token"),
    }
}

/// Compare without stopping at the first difference, so timing says nothing about the token.
fn same_token(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn result_reply(r: Result<Value, String>) -> Reply {
    match r {
        Ok(v) => json_reply(&v),
        Err(e) => (400, "application/json", json_dumps(&serde_json::json!({"error": e})).into_bytes()),
    }
}

/// A settings request (already allowed) for `method` and `path` with its body.
pub fn settings_route(host: &dyn WebHost, method: &str, path: &str, body: &[u8]) -> Reply {
    match (method, path) {
        ("GET", "/api/settings") => result_reply(host.settings()),
        ("POST", "/api/settings") => match serde_json::from_slice::<Value>(body) {
            Ok(v) => result_reply(host.apply_settings(v)),
            Err(e) => result_reply(Err(format!("not JSON: {e}"))),
        },
        ("POST", "/api/link") => result_reply(host.relink()),
        ("POST", p) if p.starts_with("/api/profiles/") => {
            let Some(agent) = host.agent() else { return (503, "text/plain; charset=utf-8", b"stopping\n".to_vec()) };
            let Some(tr) = agent.transcriber() else { return result_reply(Err("speech is off on this receiver".into())) };
            let v: Value = match serde_json::from_slice(body) {
                Ok(v) => v,
                Err(e) => return result_reply(Err(format!("not JSON: {e}"))),
            };
            let s = |k: &str| v.get(k).and_then(Value::as_str);
            match p {
                "/api/profiles/test" => {
                    let rel = s("recording").unwrap_or("").trim_start_matches("/recordings/");
                    let Some(path) = recording_path(&agent.state_dir, rel) else {
                        return result_reply(Err("no such recording".into()));
                    };
                    match crate::wav::read(&path) {
                        Ok(w) => result_reply(tr.test_profile(&w.samples(), v.get("profile").unwrap_or(&Value::Null))),
                        Err(e) => result_reply(Err(format!("can't read the recording: {e}"))),
                    }
                }
                _ => (404, "text/plain; charset=utf-8", b"not found\n".to_vec()),
            }
        }
        _ => (404, "text/plain; charset=utf-8", b"not found\n".to_vec()),
    }
}

fn handle(host: &dyn WebHost, mut req: tiny_http::Request) {
    let url = req.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((&url, ""));
    let method = req.method().to_string();
    let is_settings = (matches!(path, "/api/settings" | "/api/link") || path.starts_with("/api/profiles/"))
        && host.token().is_some();
    let (status, ctype, body) = if is_settings {
        let given = req.headers().iter().find(|h| h.field.equiv(TOKEN_HEADER)).map(|h| h.value.as_str().to_string());
        match settings_allowed(host.token().as_deref(), req.remote_addr().copied(), given.as_deref()) {
            Err(r) => r,
            Ok(()) => {
                let mut body = Vec::new();
                let read = std::io::Read::read_to_end(&mut std::io::Read::take(req.as_reader(), MAX_BODY as u64 + 1), &mut body);
                if read.is_err() || body.len() > MAX_BODY {
                    (413, "text/plain; charset=utf-8", b"request too large\n".to_vec())
                } else {
                    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| settings_route(host, &method, path, &body))) {
                        Ok(r) => r,
                        Err(_) => {
                            log::error!("settings request {path} failed");
                            (500, "text/plain; charset=utf-8", b"internal error\n".to_vec())
                        }
                    }
                }
            }
        }
    } else if *req.method() != tiny_http::Method::Get {
        // BaseHTTPRequestHandler without a do_<METHOD>
        (501, "text/html;charset=utf-8", format!("Unsupported method ('{}')", req.method()).into_bytes())
    } else {
        match host.agent() {
            None => (503, "text/plain; charset=utf-8", b"stopping\n".to_vec()),
            Some(a) => match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| route(&a, path, query))) {
                Ok(r) => r,
                Err(_) => {
                    log::error!("status page request {path} failed");
                    (500, "text/plain; charset=utf-8", b"internal error\n".to_vec())
                }
            },
        }
    };
    let header = |k: &str, v: &str| tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()).ok();
    let mut resp = tiny_http::Response::from_data(body).with_status_code(status);
    for h in [header("Content-Type", ctype), header("Cache-Control", "no-store"), header("Server", "railspotting-receiver")]
        .into_iter().flatten() {
        resp.add_header(h);
    }
    // ?download=1 saves a recording under a name that says where it came from
    if status == 200 && path.starts_with("/recordings/") && query.split('&').any(|kv| kv == "download=1") {
        let name = path["/recordings/".len()..].replace('/', "_");
        if let Some(h) = header("Content-Disposition", &format!("attachment; filename=\"{name}\"")) {
            resp.add_header(h);
        }
    }
    // page polling stays out of the journal
    let _ = req.respond(resp);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_parsing_like_parse_qs() {
        assert_eq!(query_args("kind=telemetry&limit=5"), vec![("kind".into(), "telemetry".into()), ("limit".into(), "5".into())]);
        assert_eq!(query_args("a=&b&c=x%20y+z"), vec![("c".into(), "x y z".into())]);
    }

    #[test]
    fn qr_is_svg() {
        let svg = link_qr_svg("https://railspotting.com/link?code=WDJBMJHT").unwrap();
        assert!(svg.starts_with(b"<svg"));
    }

    #[test]
    fn settings_need_host_token_loopback_and_header() {
        let local: std::net::SocketAddr = "127.0.0.1:50000".parse().unwrap();
        let lan: std::net::SocketAddr = "192.0.2.7:50000".parse().unwrap();
        assert!(settings_allowed(None, Some(local), Some("t")).is_err());          // off on this host
        assert!(settings_allowed(Some("tok"), Some(lan), Some("tok")).is_err());    // not from this machine
        assert!(settings_allowed(Some("tok"), Some(local), None).is_err());
        assert!(settings_allowed(Some("tok"), Some(local), Some("tox")).is_err());
        assert!(settings_allowed(Some("tok"), Some(local), Some("tok2")).is_err());
        assert!(settings_allowed(Some("tok"), None, Some("tok")).is_err());
        assert!(settings_allowed(Some("tok"), Some(local), Some("tok")).is_ok());
        let v6: std::net::SocketAddr = "[::1]:50000".parse().unwrap();
        assert!(settings_allowed(Some("tok"), Some(v6), Some("tok")).is_ok());
    }

    #[test]
    fn public_status_keeps_dongle_serials_only() {
        let st = public_status(&serde_json::json!({"sdrs": [{"name": "VHF", "serial": "30002222"}],
                                                   "scanner": {"model": "SR30C", "cable_serial": "01ABCDEF"}}));
        assert_eq!(st["sdrs"][0]["serial"], "30002222"); // to tell the dongles apart
        assert!(!json_dumps(&st).contains("01ABCDEF"));
    }
}
