//! The Uniden SR30C scanner: serial driver, monitor thread, focus and roles, checks, headend
//! commands, and an in-memory fake SR30C for tests and `--simulate`.

pub mod checks;
pub mod commands;
pub mod fake;
pub mod focus;
pub mod monitor;
pub mod scanner;
pub mod util;

#[path = "../../store/src/jsonfmt.rs"]
pub mod jsonfmt;

pub use commands::{CommandHost, CommandRunner};
pub use focus::{desired_role, AutoEotWatch, FocusManager, RoleManager, FOCUS_BANKS, FOCUS_CH};
pub use monitor::ScannerMonitor;
pub use scanner::{find_port, open_serial, Scanner, ScannerError, SerialLink};

use railspotting_common::Clock;
use std::sync::Arc;

/// Injectable sleep, so tests can advance a fake clock instead of waiting.
pub type Sleep = Arc<dyn Fn(f64) + Send + Sync>;

pub fn real_sleep() -> Sleep {
    Arc::new(|s: f64| {
        if s > 0.0 {
            std::thread::sleep(std::time::Duration::from_secs_f64(s))
        }
    })
}

/// A sleep that advances a fake clock instead of waiting.
pub fn fake_sleep(clock: &railspotting_common::FakeClock) -> Sleep {
    let c = clock.clone();
    Arc::new(move |s: f64| c.advance(s))
}

/// The clock and sleep a check runs with, real or fake.
#[derive(Clone)]
pub struct Timing {
    pub clock: Clock,
    pub sleep: Sleep,
}

impl Timing {
    pub fn real() -> Self {
        Timing { clock: railspotting_common::system_clock(), sleep: real_sleep() }
    }

    pub fn fake(clock: &railspotting_common::FakeClock) -> Self {
        Timing { clock: clock.clock(), sleep: fake_sleep(clock) }
    }

    pub fn now(&self) -> f64 {
        (self.clock)()
    }

    pub fn sleep(&self, s: f64) {
        (self.sleep)(s)
    }
}
