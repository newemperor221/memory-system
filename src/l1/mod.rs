//! L1 Short-Term Memory layer
//!
//! Stores cross-agent recent memories in SQLite with WAL mode.
//! Broadcasts upsert/delete events to L2 and L3 consumers.

use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, TimeZone, Utc};
use crossbeam_channel::{self as channel, Sender};
use parking_lot::Mutex;
use rusqlite::{params, Connection};
use std::collections::HashMap;

use crate::common::{Entry, Importance, Layer, L1Event, L1Consumer};
use crate::config::Config;

/// Maximum number of events buffered in the broadcast channel.
const EVENT_CHANNEL_SIZE: usize = 1024;

// ---------------------------------------------------------------------------
// L1 struct
// ---------------------------------------------------------------------------

/// Short-Term Memory layer.
///
/// Stores recent cross-agent memory in SQLite with WAL mode.
/// Broadcasts write/delete events to subscribed consumers (L2, L3).
pub struct L1 {
    /// SQLite connection (WAL mode, fsync on every write). Thread-safe via Mutex.
    db: Arc<Mutex<Connection>>,
    /// Channel sender for broadcasting upsert/delete events to L2/L3.
    event_tx: Sender<L1Event>,
    /// Channel receiver held by L1 (used to check if consumer already took it).
    event_rx: Mutex<Option<channel::Receiver<L1Event>>>,
    /// Monotonically increasing sequence counter.
    seq: Mutex<u64>,
}

impl L1 {
    /// Open (or create) the SQLite database and start the event broadcaster.
    pub fn new(config: &Config) -> Result<Self> {
        let db_path = &config.l1_path;

        // Create parent directory if needed.
        if let Some(parent) = std::path::Path::new(db_path).parent() {
            std::fs::create_dir_all(parent)?;
        }

        let conn = Connection::open(db_path)?;

        // Enable WAL mode for crash-safe writes without locking readers.
        conn.execute_batch(
            r#"
            PRAGMA journal_mode=WAL;
            PRAGMA synchronous=NORMAL;
            "#,
        )?;

        // Create schema if not exists.
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS entries (
                key       TEXT PRIMARY KEY,
                id        TEXT NOT NULL,
                value     TEXT NOT NULL,
                importance INTEGER NOT NULL DEFAULT 2,
                source    TEXT NOT NULL,
                layer     TEXT NOT NULL DEFAULT 'private',
                created_at    INTEGER NOT NULL,
                last_accessed INTEGER NOT NULL,
                expires_at    INTEGER,
                seq            INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_entries_layer ON entries(layer);
            CREATE INDEX IF NOT EXISTS idx_entries_importance ON entries(importance);
            CREATE INDEX IF NOT EXISTS idx_entries_last_accessed ON entries(last_accessed);
            "#,
        )?;

        // Bootstrap sequence number from existing rows.
        let current_seq: u64 = conn
            .query_row("SELECT COALESCE(MAX(seq), 0) FROM entries", [], |r| r.get(0))
            .unwrap_or(0);

        // Set up the event broadcast channel.
        let (tx, rx) = channel::bounded::<L1Event>(EVENT_CHANNEL_SIZE);

        let l1 = Self {
            db: Arc::new(Mutex::new(conn)),
            event_tx: tx,
            event_rx: Mutex::new(Some(rx)),
            seq: Mutex::new(current_seq),
        };

        tracing::info!("L1 initialized at {} (seq={})", db_path, current_seq);
        Ok(l1)
    }

    /// Write (or replace) an entry. Sends a broadcast event on return.
    pub fn write(&self, key: &str, entry: &Entry) -> Result<()> {
        let now = Utc::now();
        let seq = {
            let mut guard = self.seq.lock();
            *guard += 1;
            *guard
        };

        let db = self.db.lock();
        db.execute(
            r#"INSERT OR REPLACE INTO entries
               (key, id, value, importance, source, layer,
                created_at, last_accessed, expires_at, seq)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"#,
            params![
                key,
                entry.id,
                entry.value,
                entry.importance as i32,
                entry.source,
                entry.layer.to_string(),
                entry.created_at.timestamp(),
                now.timestamp(),
                entry.expires_at.map(|dt| dt.timestamp()),
                seq,
            ],
        )?;
        drop(db);

        // Broadcast event (non-blocking; drop if consumers are slow).
        let event = L1Event::Upsert {
            key: key.to_string(),
            entry: entry.clone(),
            seq,
        };
        if self.event_tx.send(event).is_err() {
            tracing::warn!("L1: event channel full, event dropped for key={}", key);
        }

        Ok(())
    }

    /// Delete an entry. Sends a broadcast event.
    pub fn delete(&self, key: &str) -> Result<()> {
        let seq = {
            let mut guard = self.seq.lock();
            *guard += 1;
            *guard
        };

        self.db.lock().execute("DELETE FROM entries WHERE key = ?1", params![key])?;

        let event = L1Event::Delete {
            key: key.to_string(),
            seq,
        };
        if self.event_tx.send(event).is_err() {
            tracing::warn!("L1: event channel full, delete event dropped for key={}", key);
        }

        Ok(())
    }

    /// Get a single entry by key.
    pub fn get(&self, key: &str) -> Option<Entry> {
        let db = self.db.lock();
        let mut stmt = db
            .prepare(
                "SELECT id, value, importance, source, layer,
                        created_at, last_accessed, expires_at
                 FROM entries WHERE key = ?1",
            )
            .ok()?;

        let mut rows = stmt.query(params![key]).ok()?;

        let row = rows.next().ok()??;

        let expires_ms: Option<i64> = row.get(7).ok();
        let entry = Entry {
            id: row.get(0).ok()?,
            key: key.to_string(),
            value: row.get(1).ok()?,
            importance: Importance::from(row.get::<_, i32>(2).ok()?),
            source: row.get(3).ok()?,
            layer: Layer::from(row.get::<_, String>(4).ok()?.as_str()),
            created_at: DateTime::from_timestamp_millis(row.get(5).ok()?).unwrap(),
            last_accessed: DateTime::from_timestamp_millis(row.get(6).ok()?).unwrap(),
            expires_at: expires_ms.map(|ms| DateTime::from_timestamp_millis(ms).unwrap()),
            tags: Vec::new(),
        };

        // Update last_accessed (write-through).
        let now = Utc::now();
        let _ = self.db.lock().execute(
            "UPDATE entries SET last_accessed = ?1 WHERE key = ?2",
            params![now.timestamp(), key],
        );

        Some(entry)
    }

    /// Full scan of all entries. Used by L2 rebuild.
    pub fn full_scan(&self) -> Vec<Entry> {
        let db = self.db.lock();
        let mut stmt = db
            .prepare(
                "SELECT key, id, value, importance, source, layer,
                        created_at, last_accessed, expires_at
                 FROM entries",
            )
            .unwrap();

        let rows = stmt
            .query_map([], |row| {
                let expires_ms: Option<i64> = row.get(8).ok();
                Ok(Entry {
                    id: row.get(1)?,
                    key: row.get::<_, String>(0)?,
                    value: row.get::<_, String>(2)?,
                    importance: Importance::from(row.get::<_, i32>(3)?),
                    source: row.get(4)?,
                    layer: Layer::from(row.get::<_, String>(5)?.as_str()),
                    created_at: DateTime::from_timestamp_millis(row.get::<_, i64>(6)?).unwrap(),
                    last_accessed: DateTime::from_timestamp_millis(row.get::<_, i64>(7)?).unwrap(),
                    expires_at: expires_ms.map(|ms| DateTime::from_timestamp_millis(ms).unwrap()),
                    tags: Vec::new(),
                })
            })
            .unwrap();

        rows.filter_map(|r| r.ok()).collect()
    }

    /// Garbage collect expired and low-importance old entries.
    ///
    /// Rule 1: Hard-expired entries (expires_at set and passed) — always delete.
    /// Rule 2: TTL-expired (last_accessed + max_age) AND not Critical — delete.
    /// Returns the number of entries deleted.
    pub fn gc(&self) -> Result<usize> {
        let now = Utc::now().timestamp();
        let max_age = 7 * 24 * 3600; // 7 days

        // Rule 1: hard-expired.
        let expired_deleted = self.db.lock().execute(
            "DELETE FROM entries WHERE expires_at IS NOT NULL AND expires_at < ?1",
            params![now],
        )?;

        // Rule 2: TTL + not Critical.
        let ttl_deleted = self.db.lock().execute(
            "DELETE FROM entries
             WHERE last_accessed < ?1
               AND importance < 4",
            params![now - max_age],
        )?;

        tracing::info!("L1 GC: {} hard-expired, {} TTL-expired", expired_deleted, ttl_deleted);
        Ok(expired_deleted + ttl_deleted)
    }

    /// Total entry count.
    pub fn len(&self) -> usize {
        self.db
            .lock()
            .query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))
            .unwrap_or(0)
    }

    /// Flush WAL checkpoint.
    pub fn flush(&self) -> anyhow::Result<()> {
        self.db.lock().execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        Ok(())
    }

    /// Check health by probing the DB.
    pub fn health_issue(&self) -> Option<String> {
        match self.db.lock().query_row("SELECT 1", [], |_| Ok(())) {
            Ok(_) => None,
            Err(e) => Some(format!("sqlite error: {}", e)),
        }
    }

    /// Keyword/prefix recall across all entries.
    pub fn recall(&self, query: &str) -> Vec<crate::common::RecallResult> {
        let query_lower = query.to_lowercase();
        self.full_scan()
            .into_iter()
            .filter(|e| {
                e.key.to_lowercase().contains(&query_lower)
                    || e.value.to_lowercase().contains(&query_lower)
            })
            .map(|e| {
                let score = if e.key.to_lowercase().contains(&query_lower) {
                    0.8
                } else {
                    0.5
                };
                crate::common::RecallResult::new(e, score, "L1")
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// L1Consumer trait implementation
// ---------------------------------------------------------------------------

impl L1Consumer for L1 {
    /// Returns the event receiver for consumers (L2/L3).
    /// Can only be called once; subsequent calls return None.
    fn take_event_rx(&self) -> Option<channel::Receiver<L1Event>> {
        self.event_rx.lock().take()
    }

    fn full_scan(&self) -> Vec<Entry> {
        L1::full_scan(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_l1() -> (L1, TempDir) {
        let tmp = TempDir::new().unwrap();
        let config = Config {
            l1_path: tmp.path().join("l1.db").to_string_lossy().to_string(),
            ..Config::default()
        };
        let l1 = L1::new(&config).unwrap();
        (l1, tmp)
    }

    #[test]
    fn write_and_get() {
        let (l1, _tmp) = test_l1();
        let entry = Entry::new(
            "private:test:001".into(),
            "hello world".into(),
            Importance::Normal,
            vec![],
            "test-agent".into(),
            Layer::Private,
        );
        l1.write("private:test:001", &entry).unwrap();
        let retrieved = l1.get("private:test:001").unwrap();
        assert_eq!(retrieved.value, "hello world");
        assert_eq!(l1.len(), 1);
    }

    #[test]
    fn delete() {
        let (l1, _tmp) = test_l1();
        let entry = Entry::new(
            "private:test:del".into(),
            "delete me".into(),
            Importance::Low,
            vec![],
            "test".into(),
            Layer::Private,
        );
        l1.write("private:test:del", &entry).unwrap();
        assert_eq!(l1.len(), 1);
        l1.delete("private:test:del").unwrap();
        assert_eq!(l1.len(), 0);
    }

    #[test]
    fn replace() {
        let (l1, _tmp) = test_l1();
        let e1 = Entry::new("private:test:x".into(), "v1".into(), Importance::Low, vec![], "a".into(), Layer::Private);
        let e2 = Entry::new("private:test:x".into(), "v2".into(), Importance::High, vec![], "b".into(), Layer::Private);
        l1.write("private:test:x", &e1).unwrap();
        l1.write("private:test:x", &e2).unwrap();
        assert_eq!(l1.len(), 1);
        assert_eq!(l1.get("private:test:x").unwrap().value, "v2");
    }

    #[test]
    fn recall() {
        let (l1, _tmp) = test_l1();
        let e1 = Entry::new("private:test:r1".into(), "apple banana".into(), Importance::Low, vec![], "a".into(), Layer::Private);
        let e2 = Entry::new("private:test:r2".into(), "car dog".into(), Importance::Low, vec![], "a".into(), Layer::Private);
        l1.write("private:test:r1", &e1).unwrap();
        l1.write("private:test:r2", &e2).unwrap();
        let results = l1.recall("apple");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].entry.value, "apple banana");
    }

    #[test]
    fn len_and_empty() {
        let (l1, _tmp) = test_l1();
        assert!(l1.is_empty());
        let e = Entry::new("k".into(), "v".into(), Importance::Low, vec![], "a".into(), Layer::Private);
        l1.write("k", &e).unwrap();
        assert_eq!(l1.len(), 1);
    }
}
