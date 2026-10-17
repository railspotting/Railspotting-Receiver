//! Durable local queue for events and reports (SQLite, WAL mode).

use crate::{jsonfmt, sql_scalar, Signal, StoreError};
use rusqlite::types::Value as Sql;
use railspotting_common::{system_clock, Clock};
use rusqlite::{params, Connection};
use serde_json::Value;
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;

pub const DEFAULT_CAP: i64 = 50_000;

pub struct Outbox {
    pub path: String,
    cap: AtomicI64,
    clock: Clock,
    db: Mutex<Option<Connection>>,
    pub wakeup: Signal,
}

fn get_db(db: &Option<Connection>) -> Result<&Connection, StoreError> {
    db.as_ref().ok_or(StoreError::Closed)
}

impl Outbox {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        Self::with(path, DEFAULT_CAP, system_clock())
    }

    pub fn with(path: impl AsRef<Path>, cap: i64, clock: Clock) -> Result<Self, StoreError> {
        let path = path.as_ref().to_string_lossy().to_string();
        let db = Connection::open(&path)?;
        db.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))?;
        db.execute_batch("PRAGMA synchronous=NORMAL")?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS items (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            kind TEXT NOT NULL,          -- 'event' or 'report'
            event_id TEXT,
            state TEXT,
            payload TEXT NOT NULL,
            created REAL NOT NULL)",
        )?;
        db.execute_batch("CREATE INDEX IF NOT EXISTS items_kind ON items(kind, id)")?;
        db.execute_batch("CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT)")?;
        Ok(Outbox { path, cap: AtomicI64::new(cap), clock, db: Mutex::new(Some(db)), wakeup: Signal::default() })
    }

    pub fn cap(&self) -> i64 {
        self.cap.load(Ordering::Relaxed)
    }

    pub fn set_cap(&self, cap: i64) {
        self.cap.store(cap, Ordering::Relaxed)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Connection>> {
        self.db.lock().unwrap_or_else(|p| p.into_inner())
    }

    // writing -----------------------------------------------------------------

    pub fn add_event(&self, event: &Value) -> Result<(), StoreError> {
        self._add("event", event, sql_scalar(event.get("event_id")), sql_scalar(event.get("state")))?;
        if event.get("state").and_then(Value::as_str) == Some("open") {
            self.wakeup.set();
        }
        Ok(())
    }

    pub fn add_report(&self, report: &Value) -> Result<(), StoreError> {
        self._add("report", report, Sql::Null, Sql::Null)
    }

    fn _add(&self, kind: &str, payload: &Value, event_id: Sql, state: Sql) -> Result<(), StoreError> {
        let g = self.lock();
        let db = get_db(&g)?;
        self.make_room(db)?;
        db.execute(
            "INSERT INTO items(kind, event_id, state, payload, created) VALUES (?,?,?,?,?)",
            params![kind, event_id, state, jsonfmt::dumps(payload), (self.clock)()],
        )?;
        Ok(())
    }

    fn make_room(&self, db: &Connection) -> Result<(), StoreError> {
        let count: i64 = db.query_row("SELECT COUNT(*) FROM items", [], |r| r.get(0))?;
        let excess = count - self.cap() + 1;
        if excess <= 0 {
            return Ok(());
        }
        // Oldest closed events go first, then anything oldest.
        let mut rows: Vec<i64> = {
            let mut st = db.prepare("SELECT id FROM items WHERE kind='event' AND state='closed' ORDER BY id LIMIT ?")?;
            let ids = st.query_map([excess], |r| r.get(0))?.collect::<Result<Vec<i64>, _>>()?;
            ids
        };
        if (rows.len() as i64) < excess {
            let not_in = if rows.is_empty() {
                "-1".to_string()
            } else {
                rows.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
            };
            let sql = format!("SELECT id FROM items WHERE id NOT IN ({not_in}) ORDER BY id LIMIT ?");
            let mut st = db.prepare(&sql)?;
            let more = st.query_map([excess - rows.len() as i64], |r| r.get(0))?.collect::<Result<Vec<i64>, _>>()?;
            rows.extend(more);
        }
        for id in &rows {
            db.execute("DELETE FROM items WHERE id=?", [id])?;
        }
        bump(db, "dropped", rows.len() as i64)
    }

    // reading -----------------------------------------------------------------

    pub fn depth(&self) -> Result<i64, StoreError> {
        let g = self.lock();
        Ok(get_db(&g)?.query_row("SELECT COUNT(*) FROM items", [], |r| r.get(0))?)
    }

    pub fn dropped(&self) -> Result<i64, StoreError> {
        let g = self.lock();
        let db = get_db(&g)?;
        let mut st = db.prepare("SELECT v FROM meta WHERE k='dropped'")?;
        let mut rows = st.query([])?;
        Ok(match rows.next()? {
            Some(r) => match r.get::<_, rusqlite::types::Value>(0)? {
                rusqlite::types::Value::Integer(i) => i,
                rusqlite::types::Value::Real(f) => f as i64,
                rusqlite::types::Value::Text(t) => t.trim().parse().unwrap_or(0),
                _ => 0,
            },
            None => 0,
        })
    }

    /// Subtract the drop count that a successful heartbeat reported.
    pub fn ack_dropped(&self, n: i64) -> Result<(), StoreError> {
        if n != 0 {
            let g = self.lock();
            bump(get_db(&g)?, "dropped", -n)?;
        }
        Ok(())
    }

    fn next_kind(&self, kind: &str, limit: i64) -> Result<Vec<(i64, Value)>, StoreError> {
        let g = self.lock();
        let db = get_db(&g)?;
        let mut st = db.prepare("SELECT id, payload FROM items WHERE kind=? ORDER BY id LIMIT ?")?;
        let rows = st
            .query_map(params![kind, limit], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        let mut out = Vec::with_capacity(rows.len());
        for (id, text) in rows {
            match serde_json::from_str(&text) {
                Ok(v) => out.push((id, v)),
                Err(e) => log::warn!("outbox row {id} is not valid JSON ({e}); skipped"),
            }
        }
        Ok(out)
    }

    pub fn next_events(&self, limit: i64) -> Result<Vec<(i64, Value)>, StoreError> {
        self.next_kind("event", limit)
    }

    pub fn next_reports(&self, limit: i64) -> Result<Vec<(i64, Value)>, StoreError> {
        self.next_kind("report", limit)
    }

    /// Delete the rows of this batch whose event_id the server listed.
    ///
    /// Deleting by row id (not by event_id) keeps a 'closed' that was queued
    /// while its 'open' was in flight.
    pub fn ack_events<S: AsRef<str>>(&self, batch: &[(i64, Value)], acked_ids: &[S]) -> Result<usize, StoreError> {
        let acked: std::collections::HashSet<&str> = acked_ids.iter().map(|s| s.as_ref()).collect();
        let rows: Vec<i64> = batch
            .iter()
            .filter(|(_, ev)| ev.get("event_id").and_then(Value::as_str).is_some_and(|e| acked.contains(e)))
            .map(|(rid, _)| *rid)
            .collect();
        let g = self.lock();
        let db = get_db(&g)?;
        for id in &rows {
            db.execute("DELETE FROM items WHERE id=?", [id])?;
        }
        Ok(rows.len())
    }

    pub fn delete(&self, row_id: i64) -> Result<(), StoreError> {
        let g = self.lock();
        get_db(&g)?.execute("DELETE FROM items WHERE id=?", [row_id])?;
        Ok(())
    }

    pub fn close(&self) {
        let mut g = self.lock();
        if let Some(db) = g.take() {
            let _ = db.close();
        }
    }
}

fn bump(db: &Connection, key: &str, n: i64) -> Result<(), StoreError> {
    db.execute(
        "INSERT INTO meta(k, v) VALUES (?, ?) ON CONFLICT(k) DO UPDATE SET v = CAST(v AS INTEGER) + ?",
        params![key, n.to_string(), n],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use railspotting_common::FakeClock;
    use serde_json::json;

    fn event(state: &str, ts: f64) -> Value {
        json!({"event_id": uuid::Uuid::new_v4().to_string(), "state": state, "start_ts": ts,
               "end_ts": if state == "open" { Value::Null } else { json!(ts + 1.0) },
               "freq": 160.95, "ch": 5, "kind": "voice", "peak_rssi": 420, "avg_rssi": 410, "samples": 3})
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("rs-store-outbox-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn eid(v: &Value) -> String {
        v["event_id"].as_str().unwrap().to_string()
    }

    #[test]
    fn ack_deletes_by_row_not_event_id() {
        let d = tmp("ack");
        let ob = Outbox::open(d.join("o.db")).unwrap();
        let op = event("open", 1.0);
        ob.add_event(&op).unwrap();
        let batch = ob.next_events(10).unwrap();
        let mut cl = op.clone();
        cl["state"] = json!("closed");
        ob.add_event(&cl).unwrap(); // queued while the open was in flight
        ob.ack_events(&batch, &[eid(&op)]).unwrap();
        let rest = ob.next_events(10).unwrap();
        assert_eq!(rest.iter().map(|(_, e)| e["state"].clone()).collect::<Vec<_>>(), vec![json!("closed")]);
        assert!(ob.wakeup.is_set());
    }

    #[test]
    fn cap_drops_oldest_closed_first() {
        let d = tmp("cap");
        let ob = Outbox::with(d.join("o.db"), 5, FakeClock::new(1.0).clock()).unwrap();
        for _ in 0..3 {
            ob.add_event(&event("open", 1.0)).unwrap();
        }
        let closes: Vec<Value> = (0..2).map(|_| event("closed", 1.0)).collect();
        for e in &closes {
            ob.add_event(e).unwrap();
        }
        ob.add_event(&event("open", 1.0)).unwrap();
        ob.add_event(&event("open", 1.0)).unwrap();
        assert_eq!(ob.depth().unwrap(), 5);
        assert_eq!(ob.dropped().unwrap(), 2);
        let left: Vec<String> = ob.next_events(10).unwrap().iter().map(|(_, e)| eid(e)).collect();
        assert!(!left.contains(&eid(&closes[0])) && !left.contains(&eid(&closes[1])));
        ob.ack_dropped(2).unwrap();
        assert_eq!(ob.dropped().unwrap(), 0);
    }

    #[test]
    fn cap_then_anything_oldest() {
        let d = tmp("cap2");
        let ob = Outbox::with(d.join("o.db"), 2, FakeClock::new(1.0).clock()).unwrap();
        for _ in 0..3 {
            ob.add_event(&event("closed", 1.0)).unwrap();
        }
        // the heartbeat body test: three closed into a cap of two drops one
        assert_eq!(ob.depth().unwrap(), 2);
        assert_eq!(ob.dropped().unwrap(), 1);
        ob.set_cap(1);
        ob.add_report(&json!({"type": "radio_check"})).unwrap();
        assert_eq!(ob.depth().unwrap(), 1);
        assert_eq!(ob.dropped().unwrap(), 3);
        assert_eq!(ob.next_reports(20).unwrap().len(), 1);
    }

    #[test]
    fn event_id_bound_as_stored_before() {
        let d = tmp("bind");
        let ob = Outbox::open(d.join("o.db")).unwrap();
        ob.add_event(&json!({"event_id": 7, "state": null})).unwrap();
        ob.add_event(&json!({"event_id": "a", "state": true})).unwrap();
        ob.add_report(&json!({"type": "x"})).unwrap();
        ob.close();
        let db = Connection::open(d.join("o.db")).unwrap();
        let rows: Vec<(Option<String>, Option<String>)> = db
            .prepare("SELECT event_id, state FROM items ORDER BY id").unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(Result::unwrap).collect();
        // what earlier receiver versions stored for the same events (TEXT affinity turns 7 and true into text)
        let s = |x: &str| Some(x.to_string());
        assert_eq!(rows, vec![(s("7"), None), (s("a"), s("1")), (None, None)]);
    }

    #[test]
    fn survives_reopen() {
        let d = tmp("reopen");
        let ob = Outbox::open(d.join("o.db")).unwrap();
        ob.add_event(&event("open", 1.0)).unwrap();
        ob.add_report(&json!({"type": "radio_check", "ts": 1, "data": {}})).unwrap();
        ob.close();
        assert!(matches!(ob.depth(), Err(StoreError::Closed)));
        let ob2 = Outbox::open(d.join("o.db")).unwrap();
        assert_eq!(ob2.depth().unwrap(), 2);
    }
}
