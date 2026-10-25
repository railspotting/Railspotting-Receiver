//! What the console shows and what keys do, without a terminal, so it can be tested.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{json, Map, Value};

pub const ROLES: [&str; 4] = ["eot", "itc", "vhf", "hot"];

pub fn role_name(r: &str) -> &str {
    match r {
        "eot" => "UHF (EOT)",
        "itc" => "220 MHz (ITC)",
        "vhf" => "VHF (detectors and voice)",
        "hot" => "UHF (HOT)",
        "off" => "Off (not used)",
        "" => "Automatic",
        other => other,
    }
}

/// A dongle's job choices, in the order Left and Right step through them.
pub const JOBS: [&str; 6] = ["", "eot", "itc", "vhf", "hot", "off"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Status,
    Events,
    Settings,
}

pub const TABS: [(Tab, &str); 3] = [(Tab::Status, "Status"), (Tab::Events, "Events"), (Tab::Settings, "Settings")];

/// The settings the console edits, as the form holds them.
#[derive(Clone, Debug, PartialEq)]
pub struct Form {
    pub label: String,
    pub other_calls: bool,
    pub watch: String,
    /// Every role, in order, and whether it is on.
    pub order: Vec<(String, bool)>,
    /// Each dongle's serial and job ("" for automatic).
    pub jobs: Vec<(String, String)>,
    /// The fixed dongles (`sdr.devices`), shown only: they change in receiver.json.
    pub devices: Vec<Value>,
}

impl Form {
    pub fn from_settings(s: &Value, connected: &[String]) -> Form {
        let sdr = s.get("sdr").cloned().unwrap_or(Value::Null);
        let on: Vec<String> = sdr.get("role_order").and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_else(|| ROLES.iter().map(|r| r.to_string()).collect());
        let mut order: Vec<(String, bool)> = on.iter().map(|r| (r.clone(), true)).collect();
        order.extend(ROLES.iter().filter(|r| !on.iter().any(|o| o == *r)).map(|r| (r.to_string(), false)));
        let assign = sdr.get("assign").and_then(Value::as_object).cloned().unwrap_or_default();
        let mut form = Form {
            label: s.get("label").and_then(Value::as_str).unwrap_or("").to_string(),
            other_calls: s.get("transcribe_other_calls").and_then(Value::as_bool).unwrap_or(false),
            watch: s.get("watch_clips").and_then(Value::as_array)
                .map(|a| a.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(", ")).unwrap_or_default(),
            order,
            jobs: assign.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string())).collect(),
            devices: sdr.get("devices").and_then(Value::as_array).cloned().unwrap_or_default(),
        };
        form.add_dongles(connected);
        form
    }

    /// Connected dongles not listed yet join as automatic; the list stays sorted.
    pub fn add_dongles(&mut self, connected: &[String]) {
        for s in connected {
            if !self.jobs.iter().any(|(j, _)| j == s) {
                self.jobs.push((s.clone(), String::new()));
            }
        }
        self.jobs.sort();
    }

    /// The change to send (`/api/settings`), or what is wrong with the form.
    pub fn patch(&self) -> Result<Value, String> {
        let mut watch = vec![];
        for part in self.watch.split(|c: char| c == ',' || c.is_whitespace()).filter(|p| !p.is_empty()) {
            watch.push(part.parse::<f64>().map_err(|_| format!("{part:?} is not a channel in MHz"))?);
        }
        let assign: Map<String, Value> = self.jobs.iter().filter(|(_, j)| !j.is_empty())
            .map(|(s, j)| (s.clone(), json!(j))).collect();
        let label = self.label.trim();
        Ok(json!({
            "label": if label.is_empty() { Value::Null } else { json!(label) },
            "transcribe_other_calls": self.other_calls,
            "watch_clips": watch,
            "sdr": {
                "role_order": self.order.iter().filter(|(_, on)| *on).map(|(r, _)| r.clone()).collect::<Vec<_>>(),
                "assign": assign,
            },
        }))
    }
}

/// One line of the Settings tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    Label,
    OtherCalls,
    Watch,
    Order(usize),
    Job(usize),
}

/// What the terminal loop should do after a key.
#[derive(Debug, PartialEq)]
pub enum Action {
    None,
    Quit,
    Refresh,
    Save(Value),
    Relink,
    EditFile,
}

pub struct App {
    pub tab: Tab,
    pub status: Option<Value>,
    /// Why the status could not be read, when it couldn't.
    pub offline: Option<String>,
    pub events: Vec<Value>,
    pub event_scroll: usize,
    /// The settings as last read from the receiver.
    pub saved: Option<Value>,
    pub form: Option<Form>,
    pub row: usize,
    /// Text being typed into the label or channel line.
    pub typing: Option<String>,
    /// A line for the user, and whether it is good news.
    pub message: Option<(String, bool)>,
    /// What the console is waiting for (saving, linking).
    pub busy: Option<String>,
    /// Quit was pressed once with unsaved changes.
    pub quit_armed: bool,
}

impl Default for App {
    fn default() -> App {
        App::new()
    }
}

impl App {
    pub fn new() -> App {
        App { tab: Tab::Status, status: None, offline: None, events: vec![], event_scroll: 0, saved: None, form: None,
              row: 0, typing: None, message: None, busy: None, quit_armed: false }
    }

    /// Serials of the dongles plugged in now, from the status.
    pub fn connected(&self) -> Vec<String> {
        let Some(st) = &self.status else { return vec![] };
        let mut out: Vec<String> = st.get("sdrs").and_then(Value::as_array).into_iter().flatten()
            .filter_map(|s| s.get("serial").and_then(Value::as_str)).filter(|s| !s.is_empty()).map(str::to_string).collect();
        for k in ["spare", "off"] {
            if let Some(a) = st.get("sdr_assign").and_then(|a| a.get(k)).and_then(Value::as_array) {
                out.extend(a.iter().filter_map(Value::as_str).map(str::to_string));
            }
        }
        out.sort();
        out.dedup();
        out
    }

    pub fn set_status(&mut self, st: Result<Value, String>) {
        match st {
            Ok(v) => {
                self.status = Some(v);
                self.offline = None;
                let connected = self.connected();
                if let Some(f) = &mut self.form {
                    f.add_dongles(&connected);
                }
            }
            Err(e) => self.offline = Some(e),
        }
    }

    /// Settings read (or saved): the form starts again from them.
    pub fn set_settings(&mut self, s: Value) {
        self.form = Some(Form::from_settings(&s, &self.connected()));
        self.saved = Some(s);
        self.row = self.row.min(self.rows().len().saturating_sub(1));
        self.quit_armed = false;
    }

    pub fn dirty(&self) -> bool {
        match (&self.saved, &self.form) {
            (Some(s), Some(f)) => Form::from_settings(s, &self.connected()) != *f,
            _ => false,
        }
    }

    pub fn rows(&self) -> Vec<Row> {
        let Some(f) = &self.form else { return vec![] };
        let mut rows = vec![Row::Label, Row::OtherCalls, Row::Watch];
        rows.extend((0..f.order.len()).map(Row::Order));
        rows.extend((0..f.jobs.len()).map(Row::Job));
        rows
    }

    pub fn current_row(&self) -> Option<Row> {
        self.rows().get(self.row).copied()
    }

    pub fn say(&mut self, text: impl Into<String>, ok: bool) {
        self.message = Some((text.into(), ok));
    }

    pub fn key(&mut self, k: KeyEvent) -> Action {
        if self.busy.is_some() {
            return Action::None;
        }
        if self.typing.is_some() {
            return self.type_key(k);
        }
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            return Action::Quit;
        }
        let quit_armed = std::mem::take(&mut self.quit_armed);
        match k.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                if self.dirty() && !quit_armed {
                    self.quit_armed = true;
                    self.say("Settings not saved. Press q again to quit without saving, or s to save.", false);
                    return Action::None;
                }
                return Action::Quit;
            }
            KeyCode::Tab | KeyCode::Right if self.tab != Tab::Settings || k.code == KeyCode::Tab => {
                self.tab = TABS[(self.tab_index() + 1) % TABS.len()].0;
                return Action::None;
            }
            KeyCode::BackTab => {
                self.tab = TABS[(self.tab_index() + TABS.len() - 1) % TABS.len()].0;
                return Action::None;
            }
            KeyCode::Left if self.tab != Tab::Settings => {
                self.tab = TABS[(self.tab_index() + TABS.len() - 1) % TABS.len()].0;
                return Action::None;
            }
            KeyCode::Char(c @ '1'..='3') => {
                self.tab = TABS[c as usize - '1' as usize].0;
                return Action::None;
            }
            KeyCode::Char('r') => return Action::Refresh,
            KeyCode::Char('e') => return Action::EditFile,
            KeyCode::Char('l') => {
                let reg = self.status.as_ref().and_then(|s| s.get("registration")).and_then(Value::as_str).unwrap_or("");
                return if reg == "approved" || reg == "checking" {
                    self.say("This receiver is already linked.", false);
                    Action::None
                } else {
                    Action::Relink
                };
            }
            _ => {}
        }
        match self.tab {
            Tab::Settings => self.settings_key(k),
            Tab::Events => {
                match k.code {
                    KeyCode::Down | KeyCode::Char('j') => self.event_scroll = (self.event_scroll + 1).min(self.events.len().saturating_sub(1)),
                    KeyCode::Up | KeyCode::Char('k') => self.event_scroll = self.event_scroll.saturating_sub(1),
                    KeyCode::PageDown => self.event_scroll = (self.event_scroll + 10).min(self.events.len().saturating_sub(1)),
                    KeyCode::PageUp => self.event_scroll = self.event_scroll.saturating_sub(10),
                    KeyCode::Home => self.event_scroll = 0,
                    _ => {}
                }
                Action::None
            }
            Tab::Status => Action::None,
        }
    }

    fn tab_index(&self) -> usize {
        TABS.iter().position(|(t, _)| *t == self.tab).unwrap_or(0)
    }

    fn type_key(&mut self, k: KeyEvent) -> Action {
        let Some(text) = &mut self.typing else { return Action::None };
        match k.code {
            KeyCode::Enter => {
                let text = self.typing.take().unwrap_or_default();
                let row = self.current_row();
                if let (Some(f), Some(row)) = (&mut self.form, row) {
                    match row {
                        Row::Label => f.label = text.chars().take(64).collect(),
                        Row::Watch => f.watch = text,
                        _ => {}
                    }
                }
            }
            KeyCode::Esc => self.typing = None,
            KeyCode::Backspace => {
                text.pop();
            }
            KeyCode::Char(c) if !c.is_control() && text.chars().count() < 200 => text.push(c),
            _ => {}
        }
        Action::None
    }

    fn settings_key(&mut self, k: KeyEvent) -> Action {
        let n = self.rows().len();
        let Some(row) = self.current_row() else { return Action::None };
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
        match k.code {
            KeyCode::Char('s') => {
                return match self.form.as_ref().map(Form::patch) {
                    Some(Ok(p)) => Action::Save(p),
                    Some(Err(e)) => {
                        self.say(format!("Not saved: {e}"), false);
                        Action::None
                    }
                    None => Action::None,
                };
            }
            KeyCode::Char('u') => {
                if let Some(s) = self.saved.clone() {
                    self.set_settings(s);
                    self.say("Changes undone.", true);
                }
                return Action::None;
            }
            KeyCode::Up | KeyCode::Char('K') if shift || k.code == KeyCode::Char('K') => self.move_role(row, -1),
            KeyCode::Down | KeyCode::Char('J') if shift || k.code == KeyCode::Char('J') => self.move_role(row, 1),
            KeyCode::Char('[') => self.move_role(row, -1),
            KeyCode::Char(']') => self.move_role(row, 1),
            KeyCode::Up | KeyCode::Char('k') => self.row = self.row.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.row = (self.row + 1).min(n.saturating_sub(1)),
            KeyCode::Left => self.step_job(row, -1),
            KeyCode::Right => self.step_job(row, 1),
            KeyCode::Enter | KeyCode::Char(' ') => {
                let Some(f) = &mut self.form else { return Action::None };
                match row {
                    Row::Label => self.typing = Some(f.label.clone()),
                    Row::Watch => self.typing = Some(f.watch.clone()),
                    Row::OtherCalls => f.other_calls = !f.other_calls,
                    Row::Order(i) => f.order[i].1 = !f.order[i].1,
                    Row::Job(_) => self.step_job(row, 1),
                }
            }
            _ => {}
        }
        Action::None
    }

    fn move_role(&mut self, row: Row, by: isize) {
        let (Some(f), Row::Order(i)) = (&mut self.form, row) else { return };
        let j = i as isize + by;
        if j < 0 || j as usize >= f.order.len() {
            return;
        }
        f.order.swap(i, j as usize);
        self.row = (self.row as isize + by) as usize;
    }

    fn step_job(&mut self, row: Row, by: isize) {
        let (Some(f), Row::Job(i)) = (&mut self.form, row) else { return };
        let at = JOBS.iter().position(|j| *j == f.jobs[i].1).unwrap_or(0) as isize;
        f.jobs[i].1 = JOBS[(at + by).rem_euclid(JOBS.len() as isize) as usize].to_string();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::from(c)
    }

    fn app() -> App {
        let mut a = App::new();
        a.set_status(Ok(json!({"registration": "approved", "sdrs": [{"role": "eot", "serial": "30001111"}, {"role": "vhf", "serial": ""}],
                                "sdr_assign": {"spare": ["30002222"], "off": []}})));
        a.set_settings(json!({"label": "Shed", "transcribe_other_calls": false, "watch_clips": [160.95],
                              "sdr": {"devices": [], "role_order": ["eot", "vhf"], "assign": {"30001111": "eot"}}}));
        a.tab = Tab::Settings;
        a
    }

    #[test]
    fn the_form_lists_every_role_and_every_dongle() {
        let a = app();
        let f = a.form.as_ref().unwrap();
        assert_eq!(f.order, [("eot".into(), true), ("vhf".into(), true), ("itc".into(), false), ("hot".into(), false)]);
        assert_eq!(f.jobs, [("30001111".into(), "eot".into()), ("30002222".into(), "".into())]);
        assert_eq!(f.watch, "160.95");
        assert!(!a.dirty());
        assert_eq!(a.rows().len(), 3 + 4 + 2);
    }

    #[test]
    fn keys_edit_the_form_and_s_saves_it() {
        let mut a = app();
        // the label: Enter, type, Enter
        a.key(key(KeyCode::Enter));
        for _ in 0..4 {
            a.key(key(KeyCode::Backspace));
        }
        for c in "Barn".chars() {
            a.key(key(KeyCode::Char(c)));
        }
        a.key(key(KeyCode::Enter));
        // switch ITC (row 5) on and move it to the top
        a.row = 5;
        a.key(key(KeyCode::Char(' ')));
        a.key(key(KeyCode::Char('[')));
        a.key(key(KeyCode::Char('[')));
        assert_eq!(a.row, 3);
        // the spare dongle (last row) goes to HOT: Automatic, eot, itc, vhf, hot
        a.row = a.rows().len() - 1;
        for _ in 0..4 {
            a.key(key(KeyCode::Right));
        }
        assert!(a.dirty());
        let Action::Save(p) = a.key(key(KeyCode::Char('s'))) else { panic!("no save") };
        assert_eq!(p, json!({"label": "Barn", "transcribe_other_calls": false, "watch_clips": [160.95],
                             "sdr": {"role_order": ["itc", "eot", "vhf"], "assign": {"30001111": "eot", "30002222": "hot"}}}));
    }

    #[test]
    fn bad_channels_are_not_saved_and_quit_asks_twice_with_changes() {
        let mut a = app();
        a.row = 2;
        a.key(key(KeyCode::Enter));
        a.key(key(KeyCode::Char('x')));
        a.key(key(KeyCode::Enter));
        assert_eq!(a.key(key(KeyCode::Char('s'))), Action::None);
        assert!(a.message.as_ref().unwrap().0.contains("not a channel"));
        assert_eq!(a.key(key(KeyCode::Char('q'))), Action::None);
        assert_eq!(a.key(key(KeyCode::Char('q'))), Action::Quit);
        a.key(key(KeyCode::Char('u')));
        assert!(!a.dirty());
        assert_eq!(a.key(key(KeyCode::Char('q'))), Action::Quit);
    }

    #[test]
    fn tabs_and_linking() {
        let mut a = app();
        a.tab = Tab::Status;
        a.key(key(KeyCode::Tab));
        assert_eq!(a.tab, Tab::Events);
        a.key(key(KeyCode::Char('3')));
        assert_eq!(a.tab, Tab::Settings);
        assert_eq!(a.key(key(KeyCode::Char('l'))), Action::None, "already linked");
        a.set_status(Ok(json!({"registration": "denied"})));
        assert_eq!(a.key(key(KeyCode::Char('l'))), Action::Relink);
        assert_eq!(a.key(key(KeyCode::Char('e'))), Action::EditFile);
    }
}
