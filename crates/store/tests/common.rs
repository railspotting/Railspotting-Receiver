#![allow(dead_code)]
use std::path::PathBuf;

/// A fresh temporary directory for one test.
pub fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rs-store-it-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Captures log lines so tests can look for the operator wording.
pub mod logcap {
    use std::sync::{Mutex, OnceLock};
    static LINES: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    struct L;
    impl log::Log for L {
        fn enabled(&self, _: &log::Metadata) -> bool { true }
        fn log(&self, r: &log::Record) {
            LINES.get_or_init(Default::default).lock().unwrap().push(format!("{} {}", r.level(), r.args()));
        }
        fn flush(&self) {}
    }
    pub fn init() {
        LINES.get_or_init(Default::default);
        let _ = log::set_logger(&L).map(|()| log::set_max_level(log::LevelFilter::Debug));
    }
    pub fn text() -> String {
        LINES.get_or_init(Default::default).lock().unwrap().join("\n")
    }
}
