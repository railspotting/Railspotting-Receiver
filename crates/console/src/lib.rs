//! The receiver's local console: a terminal screen that shows the running receiver and changes
//! its settings, like the desktop app's window does. It talks to the receiver over the control
//! socket in the state directory (`engine::control`), which only this machine's admins can open,
//! so the status page can stay read only.
//!
//! ```text
//! sudo railspotting-receiver console
//! ```
//!
//! A worker thread does the talking, so the screen keeps up while a change restarts the receiver.

pub mod app;
pub mod client;
pub mod ui;

use app::{Action, App, Tab};
use client::Client;
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use serde_json::Value;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

/// How often the status is read again.
pub const STATUS_EVERY_S: f64 = 2.0;
/// How often the events are read again while their tab shows.
pub const EVENTS_EVERY_S: f64 = 5.0;
pub const EVENTS_SHOWN: usize = 200;

enum Req {
    Status,
    Events,
    Settings,
    Save(Value),
    Relink,
}

enum Resp {
    Status(Result<Value, String>),
    Events(Result<Value, String>),
    Settings(Result<Value, String>),
    Saved(Result<Value, String>),
    Relinked(Result<Value, String>),
}

fn worker(socket: PathBuf, rx: Receiver<Req>, tx: Sender<Resp>) {
    let mut c = Client::new(&socket);
    for req in rx {
        let resp = match req {
            Req::Status => Resp::Status(c.call("GET", "/api/status", None)),
            Req::Events => Resp::Events(c.call("GET", &format!("/api/events?limit={EVENTS_SHOWN}"), None)),
            Req::Settings => Resp::Settings(c.call("GET", "/api/settings", None)),
            Req::Save(p) => Resp::Saved(c.call("POST", "/api/settings", Some(p))),
            Req::Relink => Resp::Relinked(c.call("POST", "/api/link", None)),
        };
        if tx.send(resp).is_err() {
            return;
        }
    }
}

/// Run the console until the user quits. The exit code.
pub fn run(socket: &Path, out: &mut dyn Write) -> i32 {
    let mut probe = Client::new(socket);
    if let Err(e) = probe.connect() {
        let _ = writeln!(out, "{e}");
        return 1;
    }
    drop(probe);
    let (req_tx, req_rx) = channel();
    let (resp_tx, resp_rx) = channel();
    let path = socket.to_path_buf();
    if std::thread::Builder::new().name("console-io".into()).spawn(move || worker(path, req_rx, resp_tx)).is_err() {
        let _ = writeln!(out, "could not start the console");
        return 1;
    }
    let _ = req_tx.send(Req::Status);
    let _ = req_tx.send(Req::Settings);
    let _ = req_tx.send(Req::Events);

    let mut app = App::new();
    let mut terminal = ratatui::init();
    let (mut status_at, mut events_at) = (Instant::now(), Instant::now());
    let (mut status_out, mut events_out) = (true, true);
    let result = loop {
        while let Ok(r) = resp_rx.try_recv() {
            match r {
                Resp::Status(s) => {
                    status_out = false;
                    app.set_status(s);
                }
                Resp::Events(e) => {
                    events_out = false;
                    if let Ok(Value::Array(v)) = e {
                        app.events = v;
                        app.event_scroll = app.event_scroll.min(app.events.len().saturating_sub(1));
                    }
                }
                Resp::Settings(Ok(s)) => {
                    // a refresh never throws away what is being edited
                    if !app.dirty() {
                        app.set_settings(s);
                    }
                }
                Resp::Settings(Err(e)) => app.say(format!("Could not read the settings: {e}"), false),
                Resp::Saved(r) => {
                    app.busy = None;
                    match r {
                        Ok(s) => {
                            app.set_settings(s);
                            app.say("Saved. The receiver restarted with the new settings.", true);
                        }
                        Err(e) => app.say(format!("Not saved: {e}"), false),
                    }
                    let _ = req_tx.send(Req::Status);
                }
                Resp::Relinked(r) => {
                    app.busy = None;
                    match r {
                        Ok(_) => app.say("Linking started. The code shows on the Status tab in a few seconds.", true),
                        Err(e) => app.say(e, false),
                    }
                }
            }
        }
        if !status_out && status_at.elapsed().as_secs_f64() >= STATUS_EVERY_S {
            status_out = req_tx.send(Req::Status).is_ok();
            status_at = Instant::now();
            // the settings come back when the receiver does
            if app.form.is_none() {
                let _ = req_tx.send(Req::Settings);
            }
        }
        if app.tab == Tab::Events && !events_out && events_at.elapsed().as_secs_f64() >= EVENTS_EVERY_S {
            events_out = req_tx.send(Req::Events).is_ok();
            events_at = Instant::now();
        }
        if let Err(e) = terminal.draw(|f| ui::draw(f, &app)) {
            break Err(e.to_string());
        }
        match event::poll(Duration::from_millis(200)) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(e) => break Err(e.to_string()),
        }
        let key = match event::read() {
            Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => k,
            Ok(_) => continue,
            Err(e) => break Err(e.to_string()),
        };
        app.message = None;
        match app.key(key) {
            Action::None => {}
            Action::Quit => break Ok(()),
            Action::Refresh => {
                let _ = req_tx.send(Req::Status);
                let _ = req_tx.send(Req::Events);
                if !app.dirty() {
                    let _ = req_tx.send(Req::Settings);
                }
            }
            Action::Save(p) => {
                app.busy = Some("Saving and restarting the receiver...".into());
                let _ = req_tx.send(Req::Save(p));
            }
            Action::Relink => {
                app.busy = Some("Starting to link...".into());
                let _ = req_tx.send(Req::Relink);
            }
            Action::EditFile => {
                if app.dirty() {
                    app.say("Save or undo (u) the settings first: editing receiver.json replaces them.", false);
                    continue;
                }
                ratatui::restore();
                let said = edit_file(socket);
                terminal = ratatui::init();
                let _ = terminal.clear();
                if let Some((m, ok)) = said {
                    app.say(m, ok);
                }
                let _ = req_tx.send(Req::Settings);
                let _ = req_tx.send(Req::Status);
            }
        }
    };
    ratatui::restore();
    match result {
        Ok(()) => 0,
        Err(e) => {
            let _ = writeln!(out, "the console stopped: {e}");
            1
        }
    }
}

/// The editor to use: $VISUAL, $EDITOR, else nano or vi.
pub fn editor() -> String {
    for var in ["VISUAL", "EDITOR"] {
        if let Ok(e) = std::env::var(var) {
            if !e.trim().is_empty() {
                return e;
            }
        }
    }
    let on_path = |name: &str| std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(name).is_file()));
    if on_path("nano") { "nano".into() } else { "vi".into() }
}

/// Open receiver.json in an editor and give it to the receiver, which checks it, restarts on it
/// and puts the old one back if it won't start. Runs with the screen put away. What to say after.
fn edit_file(socket: &Path) -> Option<(String, bool)> {
    let mut c = Client::new(socket);
    let mut text = match c.call("GET", "/api/config-file", None) {
        Ok(v) => v.get("text").and_then(Value::as_str).unwrap_or("{}\n").to_string(),
        Err(e) => return Some((format!("Could not read receiver.json: {e}"), false)),
    };
    let original = text.clone();
    let tmp = std::env::temp_dir().join(format!("railspotting-receiver-{}.json", std::process::id()));
    let result = loop {
        if let Err(e) = write_private(&tmp, &text) {
            break Some((format!("Could not write {}: {e}", tmp.display()), false));
        }
        let ed = editor();
        let ran = std::process::Command::new("sh").arg("-c").arg(format!("{ed} \"$1\"")).arg("sh").arg(&tmp).status();
        match ran {
            Ok(s) if s.success() => {}
            Ok(_) | Err(_) => break Some((format!("The editor ({ed}) did not finish; nothing changed. Set $EDITOR to choose another."), false)),
        }
        text = match std::fs::read_to_string(&tmp) {
            Ok(t) => t,
            Err(e) => break Some((format!("Could not read back {}: {e}", tmp.display()), false)),
        };
        if text == original {
            break Some(("receiver.json not changed.".into(), true));
        }
        let problem = match serde_json::from_str::<Value>(&text) {
            Ok(v) if v.is_object() => {
                println!("Saving receiver.json and restarting the receiver...");
                match c.call("POST", "/api/config-file", Some(serde_json::json!({"text": text}))) {
                    Ok(_) => break Some(("receiver.json saved. The receiver restarted with it.".into(), true)),
                    Err(e) => e,
                }
            }
            Ok(_) => "receiver.json must be a JSON object".into(),
            Err(e) => format!("not valid JSON: {e}"),
        };
        println!("\nNot saved: {problem}");
        if !ask("Edit it again? [Y/n] ") {
            break Some((format!("receiver.json not changed: {problem}"), false));
        }
    };
    let _ = std::fs::remove_file(&tmp);
    result
}

/// A file only this user can read: receiver.json names the headend and the dongles.
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let _ = std::fs::remove_file(path);
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
    f.write_all(text.as_bytes())
}

fn ask(q: &str) -> bool {
    print!("{q}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line).is_err() {
        return false;
    }
    !line.trim().to_lowercase().starts_with('n')
}
