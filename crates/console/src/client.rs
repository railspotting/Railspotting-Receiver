//! Talking to the running receiver over its control socket (`engine::control`): one JSON line
//! out, one back.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Long enough for a settings change, which restarts the receiver, and a profile test.
pub const TIMEOUT_S: u64 = 120;

pub struct Client {
    path: PathBuf,
    conn: Option<(UnixStream, BufReader<UnixStream>)>,
}

impl Client {
    pub fn new(path: &Path) -> Client {
        Client { path: path.to_path_buf(), conn: None }
    }

    /// Connect now, so a wrong path or missing permission shows before the screen opens.
    pub fn connect(&mut self) -> Result<(), String> {
        let s = UnixStream::connect(&self.path).map_err(|e| explain(&self.path, &e))?;
        let _ = s.set_read_timeout(Some(Duration::from_secs(TIMEOUT_S)));
        let r = s.try_clone().map_err(|e| e.to_string())?;
        self.conn = Some((s, BufReader::new(r)));
        Ok(())
    }

    /// `method` on `path`, with an optional body. The reply's body, or its error.
    pub fn call(&mut self, method: &str, path: &str, body: Option<Value>) -> Result<Value, String> {
        let mut req = json!({"method": method, "path": path});
        if let Some(b) = body {
            req["body"] = b;
        }
        let line = format!("{req}\n");
        // once more on a fresh connection: the receiver may have been restarted by systemd
        for attempt in 0..2 {
            if self.conn.is_none() {
                self.connect()?;
            }
            match self.round_trip(&line) {
                Ok(reply) => {
                    return match reply.get("status").and_then(Value::as_u64) {
                        Some(200) => Ok(reply.get("body").cloned().unwrap_or(Value::Null)),
                        _ => Err(reply.get("error").and_then(Value::as_str).unwrap_or("the receiver did not say why").to_string()),
                    };
                }
                Err(e) => {
                    self.conn = None;
                    if attempt == 1 {
                        return Err(e);
                    }
                }
            }
        }
        Err("no reply".into())
    }

    fn round_trip(&mut self, line: &str) -> Result<Value, String> {
        let (w, r) = self.conn.as_mut().ok_or("not connected")?;
        w.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
        let mut reply = String::new();
        match r.read_line(&mut reply) {
            Ok(0) => Err("the receiver closed the connection".into()),
            Ok(_) => serde_json::from_str(&reply).map_err(|e| format!("a reply that is not JSON: {e}")),
            Err(e) => Err(format!("no reply from the receiver: {e}")),
        }
    }
}

/// What to do about a socket that won't open.
fn explain(path: &Path, e: &std::io::Error) -> String {
    use std::io::ErrorKind::*;
    match e.kind() {
        NotFound | ConnectionRefused => format!("the receiver is not running ({} is missing). Start it with: sudo systemctl start railspotting-receiver",
                                                path.display()),
        PermissionDenied => format!("no permission to open {}. Run the console with sudo, or join the railspotting group \
                                     (sudo usermod -aG railspotting $USER, then log in again)", path.display()),
        _ => format!("{}: {e}", path.display()),
    }
}
