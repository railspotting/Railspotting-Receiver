//! Check for Updates: new versions come from the repository's GitHub Releases, as a bundle signed
//! with the updater key (`packaging/macos/README.md`). The app checks a minute after it starts
//! and then every six hours, and shows a found update in the menu; it installs only when asked,
//! and then stops the receiver properly before it restarts on the new version.

use crate::app::{lock, stop_engine, Core};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tauri::AppHandle;
use tauri_plugin_updater::{Update, UpdaterExt};

const FIRST_CHECK: Duration = Duration::from_secs(60);
const CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);
pub const CHECK: &str = "Check for Updates...";

fn set_item(core: &Core, text: &str, enabled: bool) {
    if let Some(t) = lock(&core.tray).as_ref() {
        let _ = t.updates.set_text(text);
        let _ = t.updates.set_enabled(enabled);
    }
}

/// Look for a newer release. `by_hand`: the menu says what was found, even "up to date".
fn check(app: &AppHandle, core: &Core, by_hand: bool) {
    if core.updating.load(Ordering::SeqCst) {
        return;
    }
    if by_hand {
        set_item(core, "Checking for Updates...", false);
    }
    match tauri::async_runtime::block_on(async { app.updater()?.check().await }) {
        Ok(Some(u)) => {
            log::info!("update {} available (this is {})", u.version, u.current_version);
            set_item(core, &format!("Install Update {} and Restart", u.version), true);
            *lock(&core.update) = Some(u);
        }
        Ok(None) => {
            *lock(&core.update) = None;
            set_item(core, if by_hand { "Up to Date: Check Again" } else { CHECK }, true);
        }
        Err(e) => {
            log::warn!("update check failed: {e}");
            set_item(core, if by_hand { "Update Check Failed: Try Again" } else { CHECK }, true);
        }
    }
}

/// Download and install `u`, then restart on it.
fn install(app: &AppHandle, core: &Arc<Core>, u: Update) {
    if core.updating.swap(true, Ordering::SeqCst) {
        return;
    }
    log::info!("installing update {}", u.version);
    set_item(core, &format!("Downloading Update {}...", u.version), false);
    let mut got = 0usize;
    let progress = |chunk: usize, total: Option<u64>| {
        got += chunk;
        if let Some(total) = total.filter(|t| *t > 0) {
            set_item(core, &format!("Downloading Update {}%", got as u64 * 100 / total), false);
        }
    };
    match tauri::async_runtime::block_on(u.download_and_install(progress, || {})) {
        Ok(()) => {
            log::info!("update {} installed; restarting", u.version);
            set_item(core, "Restarting...", false);
            // the receiver stops as on Quit (queued events get a few seconds to upload)
            core.quitting.store(true, Ordering::SeqCst);
            stop_engine(core);
            app.restart();
        }
        Err(e) => {
            log::error!("update {} did not install: {e}", u.version);
            core.updating.store(false, Ordering::SeqCst);
            set_item(core, "Update Failed: Try Again", true);
        }
    }
}

/// The menu's update item: install the update found, or look for one.
pub fn on_menu(app: &AppHandle, core: &Arc<Core>) {
    let (app, core) = (app.clone(), core.clone());
    let _ = std::thread::Builder::new().name("app-update".into()).spawn(move || {
        let found = lock(&core.update).clone();
        match found {
            Some(u) => install(&app, &core, u),
            None => check(&app, &core, true),
        }
    });
}

/// Check in the background: a minute after start, then every six hours. Not in debug builds,
/// which would offer to replace themselves with a release.
pub fn start(app: &AppHandle, core: &Arc<Core>) {
    if cfg!(debug_assertions) {
        return;
    }
    let (app, core) = (app.clone(), core.clone());
    let _ = std::thread::Builder::new().name("app-updates".into()).spawn(move || {
        let mut wait = FIRST_CHECK;
        loop {
            let until = std::time::Instant::now() + wait;
            while std::time::Instant::now() < until {
                if core.quitting.load(Ordering::SeqCst) {
                    return;
                }
                std::thread::sleep(Duration::from_secs(5));
            }
            if lock(&core.update).is_none() {
                check(&app, &core, false);
            }
            wait = CHECK_EVERY;
        }
    });
}
