//! Persistent state — SQLite. Deliberately minimal: the bot carries no state
//! git doesn't, except webhook delivery dedup and an audit log of actions.

use anyhow::Result;
use rusqlite::Connection;
use std::path::Path;
use std::sync::Mutex;

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let conn = Connection::open(dir.join("bot.sqlite"))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS deliveries (
                 id TEXT PRIMARY KEY,
                 received_at TEXT NOT NULL DEFAULT (datetime('now'))
             );
             CREATE TABLE IF NOT EXISTS events (
                 seq INTEGER PRIMARY KEY AUTOINCREMENT,
                 at TEXT NOT NULL DEFAULT (datetime('now')),
                 kind TEXT NOT NULL,
                 org TEXT,
                 detail TEXT NOT NULL
             );",
        )?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// Returns true if this delivery ID is new (and records it).
    pub fn record_delivery(&self, delivery_id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO deliveries (id) VALUES (?1)",
            [delivery_id],
        )?;
        Ok(inserted == 1)
    }

    pub fn log_event(&self, kind: &str, org: Option<&str>, detail: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO events (kind, org, detail) VALUES (?1, ?2, ?3)",
            rusqlite::params![kind, org, detail],
        )?;
        Ok(())
    }
}
