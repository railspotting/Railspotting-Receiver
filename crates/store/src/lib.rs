//! Local state: the upload outbox, event history, device identity, dongle roles and USB presence.
//!
//! The SQLite schemas and file names are those of earlier receiver versions, so an existing
//! state dir works as is.

pub mod assign;
pub mod history;
pub mod identity;
pub mod outbox;
pub mod jsonfmt;
pub mod usb;

pub use assign::{auto_mode, role_configs, slot_keys, RoleAssigner, ROLES_FILE};
pub use history::History;
pub use outbox::Outbox;

use serde_json::Value;
use std::sync::{Condvar, Mutex};

/// A JSON scalar as an SQLite value: text, integer, real, a bool as 0 or 1, anything else NULL.
pub(crate) fn sql_scalar(v: Option<&Value>) -> rusqlite::types::Value {
    use rusqlite::types::Value as S;
    match v {
        Some(Value::String(s)) => S::Text(s.clone()),
        Some(Value::Bool(b)) => S::Integer(*b as i64),
        Some(Value::Number(n)) => match n.as_i64() {
            Some(i) => S::Integer(i),
            None => S::Real(n.as_f64().unwrap_or(f64::NAN)),
        },
        _ => S::Null,
    }
}
use std::time::Duration;

#[derive(Debug)]
pub enum StoreError {
    Sql(rusqlite::Error),
    Closed,
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Sql(e) => write!(f, "{e}"),
            StoreError::Closed => f.write_str("Cannot operate on a closed database."),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Sql(e)
    }
}

/// A flag threads can wait on: set, clear, and wait with a timeout.
#[derive(Default)]
pub struct Signal {
    flag: Mutex<bool>,
    cv: Condvar,
}

impl Signal {
    pub fn set(&self) {
        *self.flag.lock().unwrap_or_else(|p| p.into_inner()) = true;
        self.cv.notify_all();
    }

    pub fn clear(&self) {
        *self.flag.lock().unwrap_or_else(|p| p.into_inner()) = false;
    }

    pub fn is_set(&self) -> bool {
        *self.flag.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Wait until set or `timeout_s` passes; returns whether it is set.
    pub fn wait(&self, timeout_s: f64) -> bool {
        let g = self.flag.lock().unwrap_or_else(|p| p.into_inner());
        let dur = Duration::from_secs_f64(timeout_s.max(0.0));
        let (g, _) = self.cv.wait_timeout_while(g, dur, |set| !*set).unwrap_or_else(|p| p.into_inner());
        *g
    }
}
