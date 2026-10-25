//! The menu bar icon and menu.
//!
//! What the icon shows comes from `view`, a plain function of the receiver's status and the
//! app's own state, so it can be tested without a menu bar.

use railspotting_engine::handle::headend_reachable;
use serde_json::Value;
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{TrayIcon, TrayIconBuilder};
use tauri::{AppHandle, Wry};

/// The three icons: macOS tints them for light and dark menu bars (template images).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Receiving,
    /// A dongle or the scanner is missing, or the receiver isn't linked.
    Attention,
    /// The headend can't be reached, or receiving is paused.
    Offline,
}

/// What the app knows besides the receiver's status.
#[derive(Debug, Clone, Default)]
pub struct Ctx {
    pub paused: bool,
    /// The receiver could not start (or restart): shown instead of the status.
    pub error: Option<String>,
    pub librtlsdr_missing: bool,
    /// Download progress of the speech model, 0 to 100.
    pub model_progress: Option<u8>,
    /// Something to mention that isn't wrong with receiving, such as a failed model download.
    pub notice: Option<String>,
    /// Whether a scanner is expected (settings don't say "none").
    pub scanner_expected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    pub icon: Icon,
    /// The disabled line at the top of the menu, and the tooltip.
    pub line: String,
}

fn view_of(icon: Icon, line: impl Into<String>) -> View {
    View { icon, line: line.into() }
}

/// The icon and the state line for `status` (what `/api/status` returns) and `ctx`.
pub fn view(status: Option<&Value>, ctx: &Ctx) -> View {
    if ctx.paused {
        return view_of(Icon::Offline, "Paused");
    }
    if let Some(e) = &ctx.error {
        return view_of(Icon::Attention, e.clone());
    }
    let Some(st) = status else { return view_of(Icon::Offline, "Starting...") };
    if ctx.librtlsdr_missing {
        return view_of(Icon::Attention, "librtlsdr missing: brew install librtlsdr");
    }
    let code = st.get("link").and_then(|l| l.get("user_code")).and_then(Value::as_str);
    match st.get("registration").and_then(Value::as_str).unwrap_or("") {
        "approved" => {}
        "waiting" => return view_of(Icon::Attention, match code {
            Some(c) => format!("Waiting for approval: code {c}"),
            None => "Waiting for approval".into(),
        }),
        "denied" => return view_of(Icon::Attention, "Link denied: open the dashboard"),
        "checking" => return view_of(Icon::Attention, "Checking the link with the headend"),
        _ => return view_of(Icon::Attention, "Not linked yet: open the dashboard"),
    }
    if headend_reachable(st) == Some(false) {
        return view_of(Icon::Offline, "Headend unreachable: events are queued");
    }
    let sdrs = st.get("sdrs").and_then(Value::as_array).cloned().unwrap_or_default();
    if sdrs.is_empty() {
        return view_of(Icon::Attention, "No dongles found");
    }
    if let Some(s) = sdrs.iter().find(|s| s.get("connected") != Some(&Value::Bool(true))) {
        let name = s.get("name").and_then(Value::as_str).unwrap_or("A dongle");
        return view_of(Icon::Attention, format!("{name} not connected"));
    }
    let scanner_on = st.get("scanner").and_then(|s| s.get("connected")) == Some(&Value::Bool(true));
    if ctx.scanner_expected && !scanner_on {
        return view_of(Icon::Attention, "Scanner not connected");
    }
    let mut line = String::from("Receiving");
    if let Some(p) = ctx.model_progress {
        line.push_str(&format!(" (speech model {p}%)"));
    } else if let Some(n) = &ctx.notice {
        line.push_str(&format!(" ({n})"));
    }
    view_of(Icon::Receiving, line)
}

pub const OPEN: &str = "open";
pub const SITE: &str = "site";
pub const WEBSITE: &str = "website";
pub const PAUSE: &str = "pause";
pub const LOGIN: &str = "login";
pub const UPDATES: &str = "updates";
pub const QUIT: &str = "quit";

/// The menu bar icon and the menu items that change.
pub struct TrayUi {
    pub tray: TrayIcon,
    pub state: MenuItem<Wry>,
    pub pause: MenuItem<Wry>,
    pub login: CheckMenuItem<Wry>,
    pub updates: MenuItem<Wry>,
    shown: Option<View>,
}

fn image(icon: Icon) -> tauri::Result<Image<'static>> {
    Image::from_bytes(match icon {
        Icon::Receiving => include_bytes!("../icons/tray.png"),
        Icon::Attention => include_bytes!("../icons/tray-attention.png"),
        Icon::Offline => include_bytes!("../icons/tray-offline.png"),
    })
}

impl TrayUi {
    pub fn build(app: &AppHandle, login_enabled: bool) -> tauri::Result<TrayUi> {
        let open = MenuItem::with_id(app, OPEN, "Open Dashboard", true, None::<&str>)?;
        let site = MenuItem::with_id(app, SITE, "Live Map", true, None::<&str>)?;
        let website = MenuItem::with_id(app, WEBSITE, "Open RailSpotting.com in Browser", true, None::<&str>)?;
        let state = MenuItem::with_id(app, "state", "Starting...", false, None::<&str>)?;
        let pause = MenuItem::with_id(app, PAUSE, "Pause Receiving", true, None::<&str>)?;
        let login = CheckMenuItem::with_id(app, LOGIN, "Launch at Login", true, login_enabled, None::<&str>)?;
        let updates = MenuItem::with_id(app, UPDATES, crate::update::CHECK, true, None::<&str>)?;
        let quit = MenuItem::with_id(app, QUIT, "Quit RailSpotting", true, None::<&str>)?;
        let menu = Menu::with_items(app, &[
            &open, &state, &PredefinedMenuItem::separator(app)?,
            &site, &website, &PredefinedMenuItem::separator(app)?,
            &pause, &login, &updates, &PredefinedMenuItem::separator(app)?,
            &quit,
        ])?;
        let tray = TrayIconBuilder::with_id("main")
            .icon(image(Icon::Offline)?)
            .icon_as_template(true)
            .tooltip("RailSpotting")
            .menu(&menu)
            .show_menu_on_left_click(true)
            .build(app)?;
        Ok(TrayUi { tray, state, pause, login, updates, shown: None })
    }

    /// Show `v`, touching the menu bar only when something changed.
    pub fn show(&mut self, v: &View, paused: bool) {
        if self.shown.as_ref() != Some(v) {
            if self.shown.as_ref().map(|s| s.icon) != Some(v.icon) {
                if let Ok(img) = image(v.icon) {
                    let _ = self.tray.set_icon(Some(img));
                    let _ = self.tray.set_icon_as_template(true);
                }
            }
            let _ = self.state.set_text(&v.line);
            let _ = self.tray.set_tooltip(Some(format!("RailSpotting: {}", v.line)));
            self.shown = Some(v.clone());
        }
        let _ = self.pause.set_text(if paused { "Resume Receiving" } else { "Pause Receiving" });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn good() -> Value {
        json!({"registration": "approved", "link": null,
               "api": {"/v1/heartbeat": {"ts": 1.0, "status": 200}},
               "sdrs": [{"name": "VHF SDR (voice)", "connected": true}, {"name": "UHF SDR (EOT)", "connected": true}],
               "scanner": {"connected": true}})
    }

    fn ctx() -> Ctx {
        Ctx { scanner_expected: true, ..Ctx::default() }
    }

    #[test]
    fn receiving() {
        assert_eq!(view(Some(&good()), &ctx()), view_of(Icon::Receiving, "Receiving"));
        let c = Ctx { model_progress: Some(42), ..ctx() };
        assert_eq!(view(Some(&good()), &c).line, "Receiving (speech model 42%)");
    }

    #[test]
    fn missing_parts_need_attention() {
        let mut st = good();
        st["sdrs"][1]["connected"] = json!(false);
        assert_eq!(view(Some(&st), &ctx()), view_of(Icon::Attention, "UHF SDR (EOT) not connected"));
        let mut st = good();
        st["sdrs"] = json!([]);
        assert_eq!(view(Some(&st), &ctx()).line, "No dongles found");
        let mut st = good();
        st["scanner"]["connected"] = json!(false);
        assert_eq!(view(Some(&st), &ctx()).icon, Icon::Attention);
        let no_scanner = Ctx { scanner_expected: false, ..ctx() };
        assert_eq!(view(Some(&st), &no_scanner).icon, Icon::Receiving);
        let lib = Ctx { librtlsdr_missing: true, ..ctx() };
        assert_eq!(view(Some(&good()), &lib).line, "librtlsdr missing: brew install librtlsdr");
    }

    #[test]
    fn linking_states() {
        let mut st = good();
        st["registration"] = json!("waiting");
        st["link"] = json!({"user_code": "WDJB-MJHT"});
        assert_eq!(view(Some(&st), &ctx()), view_of(Icon::Attention, "Waiting for approval: code WDJB-MJHT"));
        st["registration"] = json!("linking");
        assert_eq!(view(Some(&st), &ctx()).line, "Not linked yet: open the dashboard");
    }

    #[test]
    fn offline_and_paused() {
        let mut st = good();
        st["api"]["/v1/events"] = json!({"ts": 2.0, "status": null});
        assert_eq!(view(Some(&st), &ctx()).icon, Icon::Offline);
        let paused = Ctx { paused: true, ..ctx() };
        assert_eq!(view(Some(&good()), &paused), view_of(Icon::Offline, "Paused"));
        assert_eq!(view(None, &ctx()), view_of(Icon::Offline, "Starting..."));
        let err = Ctx { error: Some("could not start".into()), ..ctx() };
        assert_eq!(view(Some(&good()), &err).icon, Icon::Attention);
    }
}
