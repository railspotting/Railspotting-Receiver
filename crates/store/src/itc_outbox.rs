//! ITC records waiting for the headend (contract v1.18), in their own SQLite file
//! (`state/itc_outbox.db`), so they never count against, crowd out or change the events outbox.

use crate::{jsonfmt, StoreError};
use rusqlite::{params, Connection};
use serde_json::Value;
use std::path::Path;
use std::sync::Mutex;

/// Records kept at most; the oldest go first. A day of changes is about a thousand.
pub const ITC_CAP: i64 = 20_000;

pub struct ItcOutbox {
    db: Mutex<Option<Connection>>,
}

impl ItcOutbox {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let db = Connection::open(path.as_ref())?;
        db.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))?;
        db.execute_batch("PRAGMA synchronous=NORMAL")?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS records (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            record_id TEXT NOT NULL UNIQUE,
            t REAL NOT NULL,             -- the record's own time
            payload TEXT NOT NULL)",
        )?;
        Ok(ItcOutbox { db: Mutex::new(Some(db)) })
    }

    fn with<T>(&self, f: impl FnOnce(&Connection) -> Result<T, StoreError>) -> Result<T, StoreError> {
        let g = self.db.lock().unwrap_or_else(|p| p.into_inner());
        f(g.as_ref().ok_or(StoreError::Closed)?)
    }

    /// Queue a record; one with an `id` already queued is ignored.
    pub fn add(&self, record: &Value) -> Result<(), StoreError> {
        let id = record.get("id").and_then(Value::as_str).unwrap_or("");
        let t = record.get("t").and_then(Value::as_f64).unwrap_or(0.0);
        self.with(|db| {
            db.execute("INSERT OR IGNORE INTO records(record_id, t, payload) VALUES (?,?,?)", params![id, t, jsonfmt::dumps(record)])?;
            let count: i64 = db.query_row("SELECT COUNT(*) FROM records", [], |r| r.get(0))?;
            if count > ITC_CAP {
                db.execute("DELETE FROM records WHERE id IN (SELECT id FROM records ORDER BY id LIMIT ?)", [count - ITC_CAP])?;
            }
            Ok(())
        })
    }

    /// The oldest queued records, at most `limit`.
    pub fn next(&self, limit: i64) -> Result<Vec<Value>, StoreError> {
        self.with(|db| {
            let mut st = db.prepare("SELECT payload FROM records ORDER BY id LIMIT ?")?;
            let rows = st.query_map([limit], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows.iter().filter_map(|text| serde_json::from_str(text).ok()).collect())
        })
    }

    /// Delete the queued records with these `id`s.
    pub fn ack<S: AsRef<str>>(&self, record_ids: &[S]) -> Result<usize, StoreError> {
        self.with(|db| {
            let mut n = 0;
            for id in record_ids {
                n += db.execute("DELETE FROM records WHERE record_id=?", [id.as_ref()])?;
            }
            Ok(n)
        })
    }

    /// Delete records dated before `t` (the headend refuses them as too old).
    pub fn drop_before(&self, t: f64) -> Result<usize, StoreError> {
        self.with(|db| Ok(db.execute("DELETE FROM records WHERE t < ?", [t])?))
    }

    /// Delete every queued record (upload was turned off).
    pub fn clear(&self) -> Result<usize, StoreError> {
        self.with(|db| Ok(db.execute("DELETE FROM records", [])?))
    }

    pub fn depth(&self) -> Result<i64, StoreError> {
        self.with(|db| Ok(db.query_row("SELECT COUNT(*) FROM records", [], |r| r.get(0))?))
    }

    pub fn close(&self) {
        let mut g = self.db.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(db) = g.take() {
            let _ = db.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn records_queue_once_and_go_by_id() {
        let d = std::env::temp_dir().join(format!("rs-store-itc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let ob = ItcOutbox::open(d.join("itc_outbox.db")).unwrap();
        let rec = |id: &str, t: f64| json!({"id": id, "type": "wayside", "t": t});
        ob.add(&rec("a", 100.0)).unwrap();
        ob.add(&rec("b", 200.0)).unwrap();
        ob.add(&rec("a", 100.0)).unwrap(); // queued once
        assert_eq!(ob.depth().unwrap(), 2);
        let ids: Vec<Value> = ob.next(10).unwrap().into_iter().map(|v| v["id"].clone()).collect();
        assert_eq!(ids, vec![json!("a"), json!("b")]);
        assert_eq!(ob.drop_before(150.0).unwrap(), 1);
        assert_eq!(ob.ack(&["b", "zzz"]).unwrap(), 1);
        assert_eq!(ob.depth().unwrap(), 0);
        ob.add(&rec("c", 1.0)).unwrap();
        ob.close();
        let again = ItcOutbox::open(d.join("itc_outbox.db")).unwrap();
        assert_eq!(again.depth().unwrap(), 1, "kept across a restart");
        assert_eq!(again.clear().unwrap(), 1);
        std::fs::remove_dir_all(&d).unwrap();
    }
}
