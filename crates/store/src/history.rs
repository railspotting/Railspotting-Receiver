//! Local history for the web page: every event and a signal reading each minute (SQLite).
//!
//! Unlike the outbox, nothing here is deleted on upload; rows older than `keep_days` are pruned.

use crate::{jsonfmt, sql_scalar, StoreError};
use railspotting_common::{system_clock, Clock};
use rusqlite::{params, Connection};
use serde_json::Value;
use std::path::Path;
use std::sync::Mutex;

pub const KEEP_DAYS: f64 = 7.0;
pub const PRUNE_EVERY_S: f64 = 3600.0;

pub struct History {
    keep_s: f64,
    clock: Clock,
    next_prune: Mutex<f64>,
    db: Mutex<Option<Connection>>,
}

/// A float from the numbers, numeric strings and bools an event may carry.
fn as_float(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

fn truthy(v: Option<&Value>) -> Option<&Value> {
    match v {
        None | Some(Value::Null) | Some(Value::Bool(false)) => None,
        Some(Value::Number(n)) if n.as_f64() == Some(0.0) => None,
        Some(Value::String(s)) if s.is_empty() => None,
        other => other,
    }
}

impl History {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        Self::with(path, KEEP_DAYS, system_clock())
    }

    pub fn with(path: impl AsRef<Path>, keep_days: f64, clock: Clock) -> Result<Self, StoreError> {
        let db = Connection::open(path.as_ref())?;
        db.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))?;
        db.execute_batch("PRAGMA synchronous=NORMAL")?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS events (
            event_id TEXT PRIMARY KEY,
            start_ts REAL NOT NULL,
            kind TEXT,
            payload TEXT NOT NULL)",
        )?;
        db.execute_batch("CREATE INDEX IF NOT EXISTS events_start ON events(start_ts)")?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS signal (
            ts REAL PRIMARY KEY,
            payload TEXT NOT NULL)",
        )?;
        Ok(History { keep_s: keep_days * 86400.0, clock, next_prune: Mutex::new(0.0), db: Mutex::new(Some(db)) })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Connection>> {
        self.db.lock().unwrap_or_else(|p| p.into_inner())
    }

    // writing -----------------------------------------------------------------

    /// Insert or replace by event_id, so the closed state overwrites the open one.
    pub fn add_event(&self, event: &Value) -> Result<(), StoreError> {
        let Some(eid) = truthy(event.get("event_id")) else { return Ok(()) };
        let eid = sql_scalar(Some(eid));
        let start = truthy(event.get("start_ts")).and_then(as_float).unwrap_or_else(|| (self.clock)());
        let kind = sql_scalar(event.get("kind"));
        {
            let g = self.lock();
            let db = g.as_ref().ok_or(StoreError::Closed)?;
            db.execute("INSERT OR REPLACE INTO events VALUES (?, ?, ?, ?)",
                       params![eid, start, kind, jsonfmt::dumps(event)])?;
        }
        self.maybe_prune()
    }

    pub fn add_signal(&self, reading: &Value) -> Result<(), StoreError> {
        let Some(ts) = reading.get("ts").and_then(as_float) else {
            log::warn!("signal reading without a ts; not stored");
            return Ok(());
        };
        {
            let g = self.lock();
            let db = g.as_ref().ok_or(StoreError::Closed)?;
            db.execute("INSERT OR REPLACE INTO signal VALUES (?, ?)", params![ts, jsonfmt::dumps(reading)])?;
        }
        self.maybe_prune()
    }

    fn maybe_prune(&self) -> Result<(), StoreError> {
        let now = (self.clock)();
        {
            let mut next = self.next_prune.lock().unwrap_or_else(|p| p.into_inner());
            if now < *next {
                return Ok(());
            }
            *next = now + PRUNE_EVERY_S;
        }
        let cutoff = now - self.keep_s;
        let g = self.lock();
        let db = g.as_ref().ok_or(StoreError::Closed)?;
        db.execute("DELETE FROM events WHERE start_ts < ?", [cutoff])?;
        db.execute("DELETE FROM signal WHERE ts < ?", [cutoff])?;
        Ok(())
    }

    // reading -----------------------------------------------------------------

    pub fn events(&self, limit: i64, before: Option<f64>, kind: Option<&str>) -> Result<Vec<Value>, StoreError> {
        let mut sql = String::from("SELECT payload FROM events WHERE 1=1");
        let mut args: Vec<rusqlite::types::Value> = Vec::new();
        if let Some(b) = before {
            sql += " AND start_ts < ?";
            args.push(b.into());
        }
        if let Some(k) = kind.filter(|k| !k.is_empty()) {
            sql += " AND kind = ?";
            args.push(k.to_string().into());
        }
        sql += " ORDER BY start_ts DESC LIMIT ?";
        args.push(limit.clamp(1, 1000).into());
        let g = self.lock();
        let db = g.as_ref().ok_or(StoreError::Closed)?;
        let mut st = db.prepare(&sql)?;
        let rows = st
            .query_map(rusqlite::params_from_iter(args), |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows.iter().filter_map(|t| serde_json::from_str(t).ok()).collect())
    }

    pub fn signal(&self, since: f64) -> Result<Vec<Value>, StoreError> {
        let g = self.lock();
        let db = g.as_ref().ok_or(StoreError::Closed)?;
        let mut st = db.prepare("SELECT payload FROM signal WHERE ts >= ? ORDER BY ts")?;
        let rows = st.query_map([since], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows.iter().filter_map(|t| serde_json::from_str(t).ok()).collect())
    }

    pub fn close(&self) {
        if let Some(db) = self.lock().take() {
            let _ = db.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use railspotting_common::FakeClock;
    use serde_json::json;

    #[test]
    fn replace_prune_and_query() {
        let d = std::env::temp_dir().join(format!("rs-store-history-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let clock = FakeClock::new(1_000_000.0);
        let h = History::with(d.join("history.db"), KEEP_DAYS, clock.clock()).unwrap();
        h.add_event(&json!({"event_id": "a", "state": "open", "start_ts": 999_000.0, "kind": "voice"})).unwrap();
        h.add_event(&json!({"event_id": "a", "state": "closed", "start_ts": 999_000.0, "kind": "voice"})).unwrap();
        h.add_event(&json!({"event_id": "b", "state": "closed", "start_ts": 999_500.0, "kind": "telemetry"})).unwrap();
        h.add_event(&json!({"state": "closed"})).unwrap(); // no id: ignored
        let all = h.events(100, None, None).unwrap();
        assert_eq!(all.iter().map(|e| e["event_id"].as_str().unwrap()).collect::<Vec<_>>(), vec!["b", "a"]);
        assert_eq!(all[1]["state"], "closed");
        assert_eq!(h.events(100, None, Some("voice")).unwrap().len(), 1);
        assert_eq!(h.events(0, Some(999_400.0), None).unwrap().len(), 1);
        h.add_signal(&json!({"ts": 999_990.0, "snr_db": 30.5})).unwrap();
        assert_eq!(h.signal(999_000.0).unwrap()[0]["snr_db"], 30.5);
        // a week and an hour later the old rows go
        clock.advance(8.0 * 86400.0);
        h.add_signal(&json!({"ts": clock.get()})).unwrap();
        assert_eq!(h.events(100, None, None).unwrap().len(), 0);
        assert_eq!(h.signal(0.0).unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }
}
