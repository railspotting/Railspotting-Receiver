//! RailSpotting for macOS: the receiver engine in a menu bar app. See `MACOS_APP.md`.

// no terminal window for a release build launched from Finder
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(target_os = "macos")]
mod app;
#[cfg(target_os = "macos")]
mod here;
#[cfg(target_os = "macos")]
mod site;
#[cfg(target_os = "macos")]
mod tray;
#[cfg(target_os = "macos")]
mod update;

#[cfg(target_os = "macos")]
fn main() {
    app::run();
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("The RailSpotting desktop app is for macOS. On Linux, run railspotting-receiver.");
    std::process::exit(1);
}
