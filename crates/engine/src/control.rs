//! The local console's way in: a Unix socket in the state directory (`control.sock`).
//!
//! The status page is read only for everyone; settings change only from this machine's console
//! or the desktop app. The socket is that console's door. It is a file, so only who the file
//! system lets in can use it: the service user, its group (mode 0660) and root. A browser can't
//! reach it.
//!
//! One JSON request per line, one JSON reply per line, as many as the client likes:
//!
//! ```text
//! {"method": "GET", "path": "/api/status"}
//! {"method": "POST", "path": "/api/settings", "body": {"label": "Shed"}}
//! -> {"status": 200, "body": ...} or {"status": 400, "error": "..."}
//! ```
//!
//! Paths are the status page's JSON ones (`web::route`) and its settings ones
//! (`web::settings_route`). Pages, pictures and recordings are not offered.

use crate::web::{route, settings_route, WebHost, MAX_BODY};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

/// The socket's name in the state directory.
pub const SOCKET: &str = "control.sock";

pub struct ControlSocket {
    path: PathBuf,
    stopping: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ControlSocket {
    /// Listen on `path`. None (logged) when it can't, or when another receiver already does.
    pub fn start(host: Arc<dyn WebHost>, path: &Path) -> Option<ControlSocket> {
        if UnixStream::connect(path).is_ok() {
            log::warn!("console socket {} is in use by another receiver; this one has none", path.display());
            return None;
        }
        // left behind by a receiver that did not stop cleanly
        let _ = std::fs::remove_file(path);
        let listener = match UnixListener::bind(path) {
            Ok(l) => l,
            Err(e) => {
                log::warn!("console socket not started: {}: {e}", path.display());
                return None;
            }
        };
        // the service user and its group; until this runs, the umask keeps others from writing
        if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660)) {
            log::warn!("console socket not started: {}: {e}", path.display());
            let _ = std::fs::remove_file(path);
            return None;
        }
        let stopping = Arc::new(AtomicBool::new(false));
        let stop = stopping.clone();
        let thread = std::thread::Builder::new().name("control".into()).spawn(move || {
            for conn in listener.incoming() {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(conn) = conn else { continue };
                let host = host.clone();
                let _ = std::thread::Builder::new().name("control-conn".into()).spawn(move || serve(host.as_ref(), conn));
            }
        }).ok();
        log::info!("console socket on {}", path.display());
        Some(ControlSocket { path: path.to_path_buf(), stopping, thread })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn stop(&mut self) {
        if self.stopping.swap(true, Ordering::SeqCst) {
            return;
        }
        // wake the accept loop so it sees the flag
        let _ = UnixStream::connect(&self.path);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Drop for ControlSocket {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Requests on one connection, until the client hangs up.
fn serve(host: &dyn WebHost, conn: UnixStream) {
    let Ok(mut out) = conn.try_clone() else { return };
    let mut reader = BufReader::new(conn);
    loop {
        let mut line = String::new();
        // a little over the page's body limit: the request wraps the body
        match std::io::Read::take(&mut reader, MAX_BODY as u64 * 2).read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let reply = if line.ends_with('\n') { answer(host, line.trim()) } else { error(413, "request too long") };
        if out.write_all(format!("{reply}\n").as_bytes()).is_err() {
            return;
        }
        if !line.ends_with('\n') {
            return;
        }
    }
}

fn error(status: u16, e: &str) -> Value {
    json!({"status": status, "error": e})
}

/// The reply to one request line.
pub fn answer(host: &dyn WebHost, line: &str) -> Value {
    let req: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return error(400, &format!("not JSON: {e}")),
    };
    let method = req.get("method").and_then(Value::as_str).unwrap_or("GET");
    let Some(url) = req.get("path").and_then(Value::as_str) else { return error(400, "a request needs a path") };
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let settings = matches!(path, "/api/settings" | "/api/link" | "/api/config-file") || path.starts_with("/api/profiles/");
    let (status, kind, body) = if settings {
        let body = req.get("body").map(|b| b.to_string()).unwrap_or_default();
        settings_route(host, method, path, body.as_bytes())
    } else if method == "GET" {
        match host.agent() {
            Some(agent) => route(&agent, path, query),
            None => return error(503, "the receiver is restarting"),
        }
    } else {
        return error(405, "only GET here");
    };
    if kind != "application/json" {
        let text = String::from_utf8_lossy(&body).trim().to_string();
        return if status == 200 { error(415, "not offered on the console") } else { error(status, &text) };
    }
    let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    if status == 200 {
        json!({"status": 200, "body": v})
    } else {
        let e = v.get("error").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| format!("failed ({status})"));
        error(status, &e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Agent;

    /// A host with no agent that answers settings requests.
    struct Host;
    impl WebHost for Host {
        fn agent(&self) -> Option<Arc<Agent>> {
            None
        }
        fn settings(&self) -> Result<Value, String> {
            Ok(json!({"label": "x"}))
        }
        fn apply_settings(&self, patch: Value) -> Result<Value, String> {
            if patch.get("label").is_some() { Ok(patch) } else { Err("no label".into()) }
        }
    }

    #[test]
    fn requests_and_replies() {
        assert_eq!(answer(&Host, r#"{"path": "/api/settings"}"#), json!({"status": 200, "body": {"label": "x"}}));
        assert_eq!(answer(&Host, r#"{"method": "POST", "path": "/api/settings", "body": {"label": "y"}}"#)["body"]["label"], "y");
        assert_eq!(answer(&Host, r#"{"method": "POST", "path": "/api/settings", "body": {}}"#),
                   json!({"status": 400, "error": "no label"}));
        assert_eq!(answer(&Host, "nope")["status"], 400);
        assert_eq!(answer(&Host, r#"{"method": "GET"}"#)["status"], 400);
        assert_eq!(answer(&Host, r#"{"path": "/api/status"}"#)["status"], 503);
        assert_eq!(answer(&Host, r#"{"method": "DELETE", "path": "/api/status"}"#)["status"], 405);
    }

    #[test]
    fn the_socket_is_for_the_owner_and_its_group_and_goes_when_stopped() {
        let dir = std::env::temp_dir().join(format!("rs-control-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(SOCKET);
        let mut s = ControlSocket::start(Arc::new(Host), &path).expect("listens");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o660);
        assert!(ControlSocket::start(Arc::new(Host), &path).is_none(), "a second receiver does not take it over");

        let conn = UnixStream::connect(&path).unwrap();
        let mut w = conn.try_clone().unwrap();
        let mut r = BufReader::new(conn);
        for _ in 0..2 {
            w.write_all(b"{\"path\": \"/api/settings\"}\n").unwrap();
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            assert_eq!(serde_json::from_str::<Value>(&line).unwrap()["body"]["label"], "x");
        }
        s.stop();
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
