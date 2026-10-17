//! Log lines as receivers have always written them: "LEVEL message" to stdout (journald), with the
//! time in front on a terminal. Tests capture what one thread logs.

use std::cell::RefCell;
use std::io::{IsTerminal, Write};
use std::sync::OnceLock;

thread_local! {
    static CAPTURE: RefCell<Option<String>> = const { RefCell::new(None) };
}

struct Logger {
    tty: bool,
    print: std::sync::atomic::AtomicBool,
}

static LOGGER: OnceLock<Logger> = OnceLock::new();

impl log::Log for Logger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::max_level()
    }

    fn log(&self, r: &log::Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        // other crates (the HTTP client, the speech libraries) only from WARNING up, so their
        // chatter stays out of the journal
        if r.level() > log::Level::Warn && !r.target().starts_with("railspotting") {
            return;
        }
        let lvl = match r.level() {
            log::Level::Warn => "WARNING",
            log::Level::Trace => "DEBUG",
            l => l.as_str(),
        };
        let line = format!("{lvl} {}", r.args());
        CAPTURE.with(|c| {
            if let Some(buf) = c.borrow_mut().as_mut() {
                buf.push_str(&line);
                buf.push('\n');
            }
        });
        if self.print.load(std::sync::atomic::Ordering::Relaxed) {
            let mut out = std::io::stdout().lock();
            if self.tty {
                let now = railspotting_common::now();
                let ms = ((now * 1000.0) as i64).rem_euclid(1000);
                let _ = write!(out, "{},{ms:03} ", crate::util::local_stamp(now, "%Y-%m-%d %H:%M:%S"));
            }
            let _ = writeln!(out, "{line}");
        }
    }

    fn flush(&self) {
        let _ = std::io::stdout().flush();
    }
}

fn filter(level: &str) -> log::LevelFilter {
    match level.to_ascii_uppercase().as_str() {
        "DEBUG" => log::LevelFilter::Debug,
        "WARNING" | "WARN" => log::LevelFilter::Warn,
        "ERROR" | "CRITICAL" => log::LevelFilter::Error,
        _ => log::LevelFilter::Info,
    }
}

/// Set up logging: the level from receiver.json, the time only on a terminal.
pub fn setup(level: &str) {
    let logger = LOGGER.get_or_init(|| Logger { tty: std::io::stdout().is_terminal(), print: true.into() });
    logger.print.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = log::set_logger(logger);
    log::set_max_level(filter(level));
}

/// Tests: log at DEBUG into per-thread captures only (nothing printed).
pub fn setup_for_tests() {
    let print = std::env::var_os("RS_TEST_LOG").is_some();
    let logger = LOGGER.get_or_init(|| Logger { tty: false, print: print.into() });
    logger.print.store(print, std::sync::atomic::Ordering::Relaxed);
    if log::set_logger(logger).is_ok() || log::max_level() < log::LevelFilter::Debug {
        log::set_max_level(log::LevelFilter::Debug);
    }
}

/// Captures what this thread logs until dropped.
pub struct Capture;

impl Capture {
    pub fn start() -> Capture {
        setup_for_tests();
        CAPTURE.with(|c| *c.borrow_mut() = Some(String::new()));
        Capture
    }

    pub fn text(&self) -> String {
        CAPTURE.with(|c| c.borrow().clone().unwrap_or_default())
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        CAPTURE.with(|c| *c.borrow_mut() = None);
    }
}
