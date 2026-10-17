//! Where the desktop app keeps its files.
//!
//! The CLI keeps paths relative to `--config`. The app has no working directory to speak of, so
//! it uses the platform's per-user folders. Settings, credentials, state and models sit together
//! in the data folder, so the usual relative paths in `receiver.json` still work there.

use crate::{default_settings, merge, Settings};
use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const APP_NAME: &str = "RailSpotting";

/// The app's data and log folders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppDirs {
    /// `receiver.json`, `credentials.json`, `state/` and `models/`.
    pub data: PathBuf,
    pub logs: PathBuf,
}

impl AppDirs {
    /// The folders for a given home directory.
    #[cfg(target_os = "macos")]
    pub fn under_home(home: &Path) -> Self {
        let lib = home.join("Library");
        AppDirs { data: lib.join("Application Support").join(APP_NAME), logs: lib.join("Logs").join(APP_NAME) }
    }

    /// The folders for a given home directory (XDG defaults).
    #[cfg(not(target_os = "macos"))]
    pub fn under_home(home: &Path) -> Self {
        let name = APP_NAME.to_lowercase();
        AppDirs { data: home.join(".local/share").join(&name), logs: home.join(".local/state").join(&name).join("logs") }
    }

    /// The current user's folders, or None when there is no home directory.
    pub fn for_user() -> Option<Self> {
        std::env::var_os("HOME").filter(|h| !h.is_empty()).map(|h| AppDirs::under_home(Path::new(&h)))
    }

    pub fn config_path(&self) -> PathBuf { self.data.join("receiver.json") }
    pub fn log_path(&self) -> PathBuf { self.logs.join("receiver.log") }

    /// Make both folders.
    pub fn create(&self) -> std::io::Result<()> {
        fs::create_dir_all(&self.data)?;
        fs::create_dir_all(&self.logs)
    }
}

/// Settings that differ for the app. A laptop moves between networks, so the dashboard only
/// listens on this machine unless `receiver.json` says otherwise. The app opens the page itself,
/// so it takes any free port rather than one something else on the Mac may hold.
pub fn app_default_overrides() -> Value {
    json!({"web": {"bind": "127.0.0.1", "port": 0}})
}

impl Settings {
    /// The app's settings: `receiver.json` in the data folder, over the app defaults.
    pub fn load_app(dirs: &AppDirs) -> Result<Self, String> {
        dirs.create().map_err(|e| format!("{}: {e}", dirs.data.display()))?;
        let path = dirs.config_path();
        let file = if path.exists() {
            let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?
        } else {
            json!({})
        };
        let defaults = merge(&default_settings(), &app_default_overrides());
        Ok(Settings { raw: merge(&defaults, &file), base_dir: dirs.data.clone(), path: Some(path) })
    }
}

/// Open the app's log file for appending.
pub fn open_log(dirs: &AppDirs) -> std::io::Result<Box<dyn Write + Send>> {
    fs::create_dir_all(&dirs.logs)?;
    Ok(Box::new(fs::OpenOptions::new().create(true).append(true).open(dirs.log_path())?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rs-paths-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn mac_folders() {
        let d = AppDirs::under_home(Path::new("/h"));
        assert_eq!(d.data, PathBuf::from("/h/Library/Application Support/RailSpotting"));
        assert_eq!(d.logs, PathBuf::from("/h/Library/Logs/RailSpotting"));
        assert_eq!(d.log_path(), PathBuf::from("/h/Library/Logs/RailSpotting/receiver.log"));
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn xdg_folders() {
        let d = AppDirs::under_home(Path::new("/h"));
        assert_eq!(d.data, PathBuf::from("/h/.local/share/railspotting"));
        assert_eq!(d.logs, PathBuf::from("/h/.local/state/railspotting/logs"));
    }

    #[test]
    fn app_settings_live_in_the_data_folder() {
        let home = tmp("app");
        let dirs = AppDirs::under_home(&home);
        let s = Settings::load_app(&dirs).unwrap();
        assert!(dirs.data.is_dir() && dirs.logs.is_dir());
        assert_eq!(s.path, Some(dirs.data.join("receiver.json")));
        assert_eq!(s.state_dir(), dirs.data.join("state"));
        assert_eq!(s.models_dir(), dirs.data.join("models"));
        assert_eq!(s.credentials_path(), dirs.data.join("credentials.json"));
        assert_eq!(s.get("web")["bind"], "127.0.0.1");
        assert_eq!(s.get("web")["port"], 0);
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn app_settings_file_wins_over_app_defaults() {
        let home = tmp("file");
        let dirs = AppDirs::under_home(&home);
        dirs.create().unwrap();
        fs::write(dirs.config_path(), r#"{"web": {"bind": "0.0.0.0"}, "label": "x"}"#).unwrap();
        let s = Settings::load_app(&dirs).unwrap();
        assert_eq!(s.get("web")["bind"], "0.0.0.0");
        assert_eq!(s.str("label"), Some("x"));
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn cli_defaults_unchanged() {
        assert_eq!(Settings::from_value(&json!({}), "/r").get("web")["bind"], "0.0.0.0");
    }

    #[test]
    fn log_file_appends() {
        let home = tmp("log");
        let dirs = AppDirs::under_home(&home);
        open_log(&dirs).unwrap().write_all(b"a\n").unwrap();
        open_log(&dirs).unwrap().write_all(b"b\n").unwrap();
        assert_eq!(fs::read_to_string(dirs.log_path()).unwrap(), "a\nb\n");
        let _ = fs::remove_dir_all(home);
    }
}
