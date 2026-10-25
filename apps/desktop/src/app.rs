//! The app: runs the engine in-process, shows it in a window with two tabs (the receiver's
//! dashboard and the headend's Live Map), and lives in the menu bar when the window is closed.
//!
//! The window holds three web views: the tab bar, the dashboard and the Live Map. They stay
//! separate so the dashboard's settings token never reaches railspotting.com, and each keeps its
//! own navigation rules (see `site`).

use crate::site::{self, Route, Tab};
use crate::tray::{self, Ctx, TrayUi};
use railspotting_common::paths::{open_log, AppDirs};
use railspotting_common::{init_logging_to, Settings};
use railspotting_engine::agent::AgentOptions;
use railspotting_engine::handle::{new_token, Engine, EngineEvent, EngineOptions};
use railspotting_engine::models;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use tauri::webview::{NewWindowResponse, WebviewBuilder};
use tauri::window::WindowBuilder;
use tauri::{ActivationPolicy, AppHandle, LogicalPosition, LogicalSize, Manager, RunEvent, Url, WebviewUrl, Window, WindowEvent};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};

const WINDOW: &str = "main";
const TAB_BAR: &str = "tabs";
const DASHBOARD: &str = "dashboard";
const MAP: &str = "live-map";
/// The tab bar's height, in points.
const TAB_BAR_H: f64 = 48.0;
/// macOS's standard title bar, in points. A window with child web views lays them out from the
/// top of the frame, under the title bar, and reports the same inner and outer size.
const TITLE_BAR_H: f64 = 28.0;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Everything the app shares between the menu, the window and the engine's messages.
pub struct Core {
    dirs: AppDirs,
    /// Lets the dashboard change settings. New on every launch, never stored.
    token: String,
    engine: Mutex<Option<Engine>>,
    ctx: Mutex<Ctx>,
    status: Mutex<Option<Value>>,
    tray: Mutex<Option<TrayUi>>,
    tab: Mutex<Tab>,
    quitting: AtomicBool,
    cancel_download: AtomicBool,
}

impl Core {
    fn port(&self) -> Option<u16> {
        lock(&self.engine).as_ref().and_then(Engine::web_port)
    }

    fn dashboard_url(&self) -> Option<Url> {
        Url::parse(&format!("http://127.0.0.1:{}/", self.port()?)).ok()
    }

    /// Bring the menu bar up to date with the status and the app's own state.
    fn refresh(&self) {
        let ctx = lock(&self.ctx).clone();
        let v = tray::view(lock(&self.status).as_ref(), &ctx);
        if let Some(t) = lock(&self.tray).as_mut() {
            t.show(&v, ctx.paused);
        }
    }

    fn set_ctx(&self, f: impl FnOnce(&mut Ctx)) {
        f(&mut lock(&self.ctx));
        self.refresh();
    }
}

/// Start the engine with the settings in Application Support. Blocks for a moment.
fn start_engine(app: &AppHandle, core: &Arc<Core>) {
    let settings = match Settings::load_app(&core.dirs) {
        Ok(s) => s,
        Err(e) => {
            log::error!("{e}");
            core.set_ctx(|c| c.error = Some(format!("Settings could not be read: {e}")));
            return;
        }
    };
    let scanner_expected = settings.str("serial_port") != Some("none") && settings.str("scanner_role") != Some("none");
    let librtlsdr_missing = railspotting_engine::rtl::librtlsdr().is_err();
    if librtlsdr_missing {
        log::warn!("librtlsdr not found; install it with: brew install librtlsdr");
    }
    let opts = EngineOptions { agent: AgentOptions::default(), token: Some(core.token.clone()), control: None };
    match Engine::start(settings, opts) {
        Ok(engine) => {
            let rx = engine.subscribe();
            *lock(&core.status) = Some(engine.status());
            *lock(&core.engine) = Some(engine);
            core.set_ctx(|c| {
                c.error = None;
                c.scanner_expected = scanner_expected;
                c.librtlsdr_missing = librtlsdr_missing;
            });
            let me = core.clone();
            let _ = std::thread::Builder::new().name("app-events".into()).spawn(move || {
                for ev in rx.iter() {
                    match ev {
                        EngineEvent::Status(st) => {
                            *lock(&me.status) = Some(st);
                            me.refresh();
                        }
                        EngineEvent::Error(e) => me.set_ctx(|c| c.error = Some(e)),
                        EngineEvent::Stopped => break,
                        EngineEvent::Event(_) | EngineEvent::Link(_) => {}
                    }
                }
            });
            // an open dashboard follows the engine to its new port
            if let (Some(w), Some(url)) = (app.get_webview(DASHBOARD), core.dashboard_url()) {
                let _ = w.navigate(url);
            }
        }
        Err(e) => {
            log::error!("receiver did not start: {e}");
            core.set_ctx(|c| c.error = Some(format!("Receiver did not start: {e}")));
        }
    }
}

fn stop_engine(core: &Core) {
    let engine = lock(&core.engine).take();
    if let Some(e) = engine {
        e.stop();
    }
    *lock(&core.status) = None;
    core.refresh();
}

/// Open the window (or bring it forward), with the Dock icon while it shows, on `tab` when
/// given and otherwise on the tab it last showed.
fn show_window(app: &AppHandle, core: &Arc<Core>, tab: Option<Tab>) {
    let _ = app.set_activation_policy(ActivationPolicy::Regular);
    let window = match app.get_window(WINDOW) {
        Some(w) => w,
        None => match build_window(app, core) {
            Ok(w) => w,
            Err(e) => {
                log::error!("window did not open: {e}");
                return;
            }
        },
    };
    let _ = window.show();
    let _ = window.unminimize();
    let _ = window.set_focus();
    let tab = tab.unwrap_or(*lock(&core.tab));
    select_tab(app, core, tab);
}

/// The window, its tab bar and the dashboard. The Live Map is added the first time it is shown.
fn build_window(app: &AppHandle, core: &Arc<Core>) -> tauri::Result<Window> {
    let window = WindowBuilder::new(app, WINDOW)
        .title("RailSpotting")
        .inner_size(1180.0, 860.0)
        .min_inner_size(420.0, 500.0)
        .build()?;
    let (w, h) = (1180.0, 860.0);

    let (tab_app, tab_core) = (app.clone(), core.clone());
    let tabs = WebviewBuilder::new(TAB_BAR, WebviewUrl::App("tabs.html".into()))
        .on_navigation(move |url| {
            if let Some(t) = site::tab_request(url) {
                // switching shows and hides web views; do it outside the navigation callback
                let (app, core) = (tab_app.clone(), tab_core.clone());
                let _ = std::thread::Builder::new().name("tab".into()).spawn(move || select_tab(&app, &core, t));
                return false;
            }
            follow(&tab_app, &tab_core, site::from_tab_bar(url), url)
        });
    window.add_child(tabs, LogicalPosition::new(0.0, 0.0), LogicalSize::new(w, TAB_BAR_H))?;

    let url = core.dashboard_url().map(WebviewUrl::External).unwrap_or_else(|| WebviewUrl::App("index.html".into()));
    // the page reads the token from here; it is only ever loaded from 127.0.0.1 (see on_navigation)
    let script = format!("window.RAILSPOTTING_TOKEN = {};", serde_json::to_string(&core.token).unwrap_or_default());
    let (nav, new_win) = (core.clone(), core.clone());
    let (nav_app, new_app) = (app.clone(), app.clone());
    let dashboard = WebviewBuilder::new(DASHBOARD, url)
        .initialization_script(script)
        .on_navigation(move |url| follow(&nav_app, &nav, site::from_dashboard(url, nav.port()), url))
        .on_new_window(move |url, _| {
            let route = match site::from_dashboard(&url, new_win.port()) {
                Route::Stay => Route::Drop, // the dashboard has one view
                r => r,
            };
            follow(&new_app, &new_win, route, &url);
            NewWindowResponse::Deny
        });
    window.add_child(dashboard, LogicalPosition::new(0.0, TAB_BAR_H), LogicalSize::new(w, h - TAB_BAR_H))?;

    let resize_app = app.clone();
    window.on_window_event(move |ev| {
        if matches!(ev, WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. }) {
            layout(&resize_app);
        }
    });
    Ok(window)
}

/// Fit the tab bar across the top, below the title bar, and the views below it.
fn layout(app: &AppHandle) {
    let Some(window) = app.get_window(WINDOW) else { return };
    let (Ok(inner), Ok(scale)) = (window.inner_size(), window.scale_factor()) else { return };
    let size = inner.to_logical::<f64>(scale);
    let title = TITLE_BAR_H;
    let below = LogicalSize::new(size.width, (size.height - title - TAB_BAR_H).max(0.0));
    if let Some(t) = app.get_webview(TAB_BAR) {
        let _ = t.set_position(LogicalPosition::new(0.0, title));
        let _ = t.set_size(LogicalSize::new(size.width, TAB_BAR_H));
    }
    for label in [DASHBOARD, MAP] {
        if let Some(v) = app.get_webview(label) {
            let _ = v.set_position(LogicalPosition::new(0.0, title + TAB_BAR_H));
            let _ = v.set_size(below);
        }
    }
}

/// Show one view and hide the other.
fn select_tab(app: &AppHandle, core: &Arc<Core>, tab: Tab) {
    let Some(window) = app.get_window(WINDOW) else { return };
    if tab == Tab::Map {
        ensure_map(app, core, &window);
    }
    let (show, hide) = match tab {
        Tab::Receiver => (DASHBOARD, MAP),
        Tab::Map => (MAP, DASHBOARD),
    };
    if let Some(v) = app.get_webview(show) {
        let _ = v.show();
        let _ = v.set_focus();
    }
    if let Some(v) = app.get_webview(hide) {
        let _ = v.hide();
    }
    *lock(&core.tab) = tab;
    if let Some(t) = app.get_webview(TAB_BAR) {
        let _ = t.eval(format!("setTab({:?})", tab.name()));
    }
    layout(app);
}

/// This receiver's device ID, once it is linked.
fn device_id(core: &Core) -> Option<String> {
    lock(&core.status).as_ref().and_then(|s| s.get("device_id")).and_then(Value::as_str).map(str::to_string)
}

/// The Live Map view: the headend's page made for the app, for this receiver. It never gets the
/// dashboard's token, and only loads that page (see `site`).
fn ensure_map(app: &AppHandle, core: &Arc<Core>, window: &Window) {
    let url = site::live_map_url(device_id(core).as_deref());
    if let Some(v) = app.get_webview(MAP) {
        // shown before the receiver was linked: now it can show this receiver
        let shows_receiver = v.url().ok().is_some_and(|u| u.query_pairs().any(|(k, _)| k == "receiver"));
        if !shows_receiver && url.query().is_some() {
            let _ = v.navigate(url);
        }
        return;
    }
    let (nav_app, new_app) = (app.clone(), app.clone());
    let (nav_core, new_core) = (core.clone(), core.clone());
    let map = WebviewBuilder::new(MAP, WebviewUrl::External(url))
        .on_navigation(move |url| follow(&nav_app, &nav_core, site::from_live_map(url), url))
        .on_new_window(move |url, _| {
            follow(&new_app, &new_core, site::new_window_from_live_map(&url), &url);
            NewWindowResponse::Deny
        });
    match window.add_child(map, LogicalPosition::new(0.0, TAB_BAR_H), LogicalSize::new(800.0, 600.0)) {
        Ok(v) => {
            let _ = v.with_webview(|w| crate::here::allow_location(w.inner()));
        }
        Err(e) => log::error!("Live Map did not open: {e}"),
    }
}

/// Act on a route; true lets the view load the page itself.
fn follow(app: &AppHandle, core: &Arc<Core>, route: Route, url: &Url) -> bool {
    match route {
        Route::Stay => true,
        Route::LiveMap => {
            // switching shows and hides web views; do it outside the navigation callback
            let (app, core) = (app.clone(), core.clone());
            let _ = std::thread::Builder::new().name("open-map".into()).spawn(move || show_window(&app, &core, Some(Tab::Map)));
            false
        }
        Route::Browser => {
            open_in_browser(url);
            false
        }
        Route::Drop => false,
    }
}

/// Links that leave the app's pages open in the browser.
fn open_in_browser(url: &Url) {
    if matches!(url.scheme(), "http" | "https") {
        let _ = std::process::Command::new("/usr/bin/open").arg(url.as_str()).spawn();
    }
}

fn hide_window(app: &AppHandle) {
    if let Some(w) = app.get_window(WINDOW) {
        let _ = w.hide();
    }
    // no Dock icon while only the menu bar icon is left
    let _ = app.set_activation_policy(ActivationPolicy::Accessory);
}

/// Download the speech model the first time, then restart the receiver so it uses it.
fn ensure_model(app: &AppHandle, core: &Arc<Core>) {
    if !cfg!(feature = "metal") {
        return;
    }
    let Ok(settings) = Settings::load_app(&core.dirs) else { return };
    if !settings.bool("transcription", true) || models::is_present(&settings.models_dir(), &models::SPEECH_MODEL) {
        return;
    }
    let (app, core) = (app.clone(), core.clone());
    let _ = std::thread::Builder::new().name("model-download".into()).spawn(move || {
        log::info!("downloading the speech model {}", models::SPEECH_MODEL.name);
        let mut last = None;
        let mut progress = |done: u64, total: u64| {
            let pct = (done * 100 / total.max(1)).min(100) as u8;
            if last != Some(pct) {
                last = Some(pct);
                core.set_ctx(|c| c.model_progress = Some(pct));
            }
        };
        let r = models::download(&settings.models_dir(), &models::SPEECH_MODEL, &mut progress, &core.cancel_download);
        core.set_ctx(|c| c.model_progress = None);
        match r {
            Ok(p) => {
                log::info!("speech model saved to {}", p.display());
                if !lock(&core.ctx).paused && !core.quitting.load(Ordering::SeqCst) {
                    stop_engine(&core);
                    start_engine(&app, &core);
                }
            }
            Err(e) => {
                log::error!("speech model download failed: {e}");
                core.set_ctx(|c| c.notice = Some("speech model not downloaded".into()));
            }
        }
    });
}

fn toggle_pause(app: &AppHandle, core: &Arc<Core>) {
    let pause = !lock(&core.ctx).paused;
    core.set_ctx(|c| c.paused = pause);
    let (app, core) = (app.clone(), core.clone());
    // stopping waits for the dongles and a short upload; keep it off the main thread
    let _ = std::thread::Builder::new().name("app-pause".into()).spawn(move || {
        if pause {
            log::info!("receiving paused");
            stop_engine(&core);
        } else {
            log::info!("receiving resumed");
            start_engine(&app, &core);
        }
    });
}

fn toggle_login(app: &AppHandle, core: &Arc<Core>) {
    let al = app.autolaunch();
    let want = !al.is_enabled().unwrap_or(false);
    let r = if want { al.enable() } else { al.disable() };
    if let Err(e) = r {
        log::error!("launch at login: {e}");
    }
    let now = al.is_enabled().unwrap_or(false);
    if let Some(t) = lock(&core.tray).as_ref() {
        let _ = t.login.set_checked(now);
    }
}

/// Stop the engine (it sends what is queued and closes the dongles), then exit.
fn quit(app: &AppHandle, core: &Arc<Core>) {
    if core.quitting.swap(true, Ordering::SeqCst) {
        return;
    }
    core.cancel_download.store(true, Ordering::SeqCst);
    hide_window(app);
    if let Some(t) = lock(&core.tray).as_ref() {
        let _ = t.state.set_text("Stopping...");
    }
    let (app, core) = (app.clone(), core.clone());
    let _ = std::thread::Builder::new().name("app-quit".into()).spawn(move || {
        log::info!("quitting");
        stop_engine(&core);
        app.exit(0);
    });
}

/// `--live-map` on the command line: open on the Live Map tab.
fn tab_arg(args: &[String]) -> Option<Tab> {
    args.iter().any(|a| a == "--live-map").then_some(Tab::Map)
}

pub fn run() {
    let Some(dirs) = AppDirs::for_user() else {
        eprintln!("no home folder");
        std::process::exit(1);
    };
    if let Err(e) = dirs.create() {
        eprintln!("{}: {e}", dirs.data.display());
        std::process::exit(1);
    }
    let level = Settings::load_app(&dirs).ok().and_then(|s| s.str("log_level").map(str::to_string)).unwrap_or_else(|| "INFO".into());
    match open_log(&dirs) {
        Ok(f) => init_logging_to(&level, f, true),
        Err(_) => railspotting_common::init_logging(&level),
    }
    log::info!("RailSpotting app {} starting; data in {}", env!("CARGO_PKG_VERSION"), dirs.data.display());

    let core = Arc::new(Core {
        dirs,
        token: new_token(),
        engine: Mutex::new(None),
        ctx: Mutex::new(Ctx::default()),
        status: Mutex::new(None),
        tray: Mutex::new(None),
        tab: Mutex::new(Tab::Receiver),
        quitting: AtomicBool::new(false),
        cancel_download: AtomicBool::new(false),
    });

    let single = core.clone();
    let app = tauri::Builder::default()
        // a second launch opens the window of the first
        // a second launch opens the window of the first; with --live-map, on that tab
        .plugin(tauri_plugin_single_instance::init(move |app, args, _| show_window(app, &single, tab_arg(&args))))
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, None))
        .setup({
            let core = core.clone();
            move |app| {
                let handle = app.handle().clone();
                let login = handle.autolaunch().is_enabled().unwrap_or(false);
                *lock(&core.tray) = Some(TrayUi::build(&handle, login)?);
                core.refresh();
                let menu_core = core.clone();
                handle.on_menu_event(move |app, ev| match ev.id().as_ref() {
                    tray::OPEN => show_window(app, &menu_core, Some(Tab::Receiver)),
                    tray::SITE => show_window(app, &menu_core, Some(Tab::Map)),
                    tray::WEBSITE => {
                        if let Ok(u) = Url::parse(site::SITE) {
                            open_in_browser(&u);
                        }
                    }
                    tray::PAUSE => toggle_pause(app, &menu_core),
                    tray::LOGIN => toggle_login(app, &menu_core),
                    tray::QUIT => quit(app, &menu_core),
                    _ => {}
                });
                start_engine(&handle, &core);
                ensure_model(&handle, &core);
                let args: Vec<String> = std::env::args().collect();
                show_window(&handle, &core, Some(tab_arg(&args).unwrap_or(Tab::Receiver)));
                Ok(())
            }
        })
        .build(tauri::generate_context!());
    let app = match app {
        Ok(a) => a,
        Err(e) => {
            log::error!("app did not start: {e}");
            std::process::exit(1);
        }
    };
    app.run(move |handle, event| match event {
        RunEvent::WindowEvent { label, event: WindowEvent::CloseRequested { api, .. }, .. } if label == WINDOW => {
            // closing the window keeps the receiver running in the menu bar
            api.prevent_close();
            hide_window(handle);
        }
        RunEvent::ExitRequested { code: None, api, .. } => {
            // the last window closed: the receiver keeps running in the menu bar
            api.prevent_exit();
        }
        RunEvent::ExitRequested { api, .. } if !core.quitting.load(Ordering::SeqCst) => {
            // Cmd-Q and log out: stop the receiver properly first
            api.prevent_exit();
            quit(handle, &core);
        }
        RunEvent::Exit if !core.quitting.swap(true, Ordering::SeqCst) => {
            // macOS's own terminate (the app menu's Quit, a quit Apple event, log out) ends the
            // loop without ExitRequested: stop the receiver here, before the process goes
            log::info!("quitting");
            core.cancel_download.store(true, Ordering::SeqCst);
            stop_engine(&core);
        }
        RunEvent::Reopen { .. } => show_window(handle, &core, None),
        _ => {}
    });
}
