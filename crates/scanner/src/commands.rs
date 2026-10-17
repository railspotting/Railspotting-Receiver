//! Runs headend commands (API contract v1 command types).

use crate::checks;
use crate::monitor::ScannerMonitor;
use crate::scanner::ScannerError;
use crate::util::{value_i64, repr_text, value_str_opt};
use crate::Timing;
use serde_json::{json, Value};
use std::path::PathBuf;

pub const NEEDS_SCANNER: [&str; 6] = ["reception_test", "detection_test", "report_channel_memory", "set_volume",
                                      "set_squelch", "resume_scan"];

/// What a command needs from the receiver.
pub trait CommandHost {
    fn monitor(&self) -> &ScannerMonitor;
    fn state_dir(&self) -> PathBuf;
    /// The current channel plan.
    fn config(&self) -> Value;
    /// The agent's radio check (scanner, else SDR reference); reports it when `send`.
    fn radio_check(&self, send: bool) -> Result<Value, ScannerError>;
    /// Queue a report for the headend.
    fn report(&self, rtype: &str, data: Value);
    /// Clock and sleep for the checks; the monitor's by default.
    fn timing(&self) -> Timing {
        self.monitor().timing()
    }
}

pub struct CommandRunner;

fn fail(error: impl Into<String>) -> Value {
    json!({"ok": false, "data": null, "error": error.into()})
}

impl CommandRunner {
    pub fn run(host: &dyn CommandHost, cmd: &Value) -> Value {
        let ctype = cmd.get("type");
        let ctype_s = ctype.and_then(Value::as_str).unwrap_or("");
        let args = cmd.get("args").filter(|a| crate::util::truthy(Some(a))).cloned().unwrap_or_else(|| json!({}));
        log::info!("command {}: {}", value_str_opt(cmd.get("id")), value_str_opt(ctype));
        let m = host.monitor();
        if NEEDS_SCANNER.contains(&ctype_s) && !m.connected() {
            return fail("no scanner connected");
        }
        let r: Result<Option<Value>, ScannerError> = (|| {
            Ok(Some(match ctype_s {
                "radio_check" => {
                    let data = host.radio_check(true)?;
                    json!({"ok": crate::util::truthy(data.get("ok")), "data": data, "error": data.get("error")})
                }
                "reception_test" => {
                    let data = checks::reception_test(m, &host.state_dir(), &host.config(), 4.0, &host.timing())?;
                    host.report("reception_test", data.clone());
                    json!({"ok": true, "data": data, "error": null})
                }
                "detection_test" => {
                    let data = checks::detection_test(m, &host.state_dir(), &host.config(), checks::DETECT_TIMEOUT_S, &host.timing())?;
                    host.report("detection_test", data.clone());
                    json!({"ok": data["ok"], "data": data, "error": data.get("error")})
                }
                "report_channel_memory" => {
                    let data = checks::channel_memory(m)?;
                    host.report("channel_memory", data.clone());
                    json!({"ok": true, "data": data, "error": null})
                }
                "set_volume" | "set_squelch" => {
                    let level = value_i64(args.get("level")).map_err(ScannerError::Value)?;
                    if !(0..=15).contains(&level) {
                        return Ok(Some(fail("level must be 0-15")));
                    }
                    m.borrow(ctype_s, |s| {
                        if ctype_s == "set_volume" {
                            s.set_volume(level)?;
                            m.set_volume(level);
                        } else {
                            s.set_squelch(level)?;
                            m.set_squelch(level);
                        }
                        Ok(())
                    })?;
                    json!({"ok": true, "data": {"level": level}, "error": null})
                }
                "resume_scan" => {
                    m.borrow("resume scan", |s| s.resume_scan())?;
                    json!({"ok": true, "data": {}, "error": null})
                }
                _ => return Ok(None),
            }))
        })();
        match r {
            Ok(Some(v)) => v,
            Ok(None) => fail(format!("unknown command type {}", match ctype {
                Some(Value::String(s)) => repr_text(s),
                other => value_str_opt(other),
            })),
            Err(ScannerError::Io(e)) => fail(e),
            Err(ScannerError::Value(e)) => fail(format!("bad arguments: {e}")),
        }
    }
}
