//! Event log — every action tagged with its causing commit (§12 principles).
//! The reconciler carries no state git doesn't; this is an audit trail, not
//! a source of truth.

use anyhow::Result;
use rusqlite::Connection;
use std::path::Path;
use std::sync::Mutex;

pub struct Store {
    conn: Mutex<Connection>,
}

#[derive(Debug, serde::Serialize)]
pub struct Event {
    pub at: String,
    pub commit: String,
    pub project: String,
    pub node: String,
    pub action: String,
    pub result: String,
}

impl Store {
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let conn = Connection::open(dir.join("reconciler.sqlite"))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS events (
                 seq INTEGER PRIMARY KEY AUTOINCREMENT,
                 at TEXT NOT NULL DEFAULT (datetime('now')),
                 commit_sha TEXT NOT NULL,
                 project TEXT NOT NULL,
                 node TEXT NOT NULL,
                 action TEXT NOT NULL,
                 result TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS ephemeral_stacks (
                 project TEXT NOT NULL,
                 app TEXT NOT NULL,
                 first_deployed TEXT NOT NULL DEFAULT (datetime('now')),
                 missing_since TEXT,
                 PRIMARY KEY (project, app)
             );",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn record(
        &self,
        commit: &str,
        project: &str,
        node: &str,
        action: &str,
        result: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO events (commit_sha, project, node, action, result) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![commit, project, node, action, result],
        )?;
        Ok(())
    }

    // ── Ephemeral lifecycle tracking (§8: 48 h grace, 7 d hard TTL) ────────

    /// Manifest present this cycle: (re)register, clear any grace countdown.
    pub fn ephemeral_mark_seen(&self, project: &str, app: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO ephemeral_stacks (project, app) VALUES (?1, ?2)
             ON CONFLICT (project, app) DO UPDATE SET missing_since = NULL",
            [project, app],
        )?;
        Ok(())
    }

    /// Container exists but manifest is gone: start (or keep) the countdown.
    pub fn ephemeral_mark_missing(&self, project: &str, app: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO ephemeral_stacks (project, app, missing_since) VALUES (?1, ?2, datetime('now'))
             ON CONFLICT (project, app) DO UPDATE
             SET missing_since = COALESCE(ephemeral_stacks.missing_since, datetime('now'))",
            [project, app],
        )?;
        Ok(())
    }

    /// Deployed more than 7 days ago — hard TTL, regardless of the manifest.
    pub fn ephemeral_ttl_expired(&self, project: &str, app: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let expired = conn
            .query_row(
                "SELECT 1 FROM ephemeral_stacks
                 WHERE project = ?1 AND app = ?2 AND first_deployed < datetime('now', '-7 days')",
                [project, app],
                |_| Ok(()),
            )
            .is_ok();
        Ok(expired)
    }

    /// Apps whose 48 h post-close grace has run out.
    pub fn ephemeral_grace_expired(&self, project: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT app FROM ephemeral_stacks
             WHERE project = ?1 AND missing_since < datetime('now', '-48 hours')",
        )?;
        let apps = stmt
            .query_map([project], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        Ok(apps)
    }

    pub fn ephemeral_forget(&self, project: &str, app: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM ephemeral_stacks WHERE project = ?1 AND app = ?2",
            [project, app],
        )?;
        Ok(())
    }

    pub fn recent(&self, limit: u32) -> Result<Vec<Event>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT at, commit_sha, project, node, action, result FROM events ORDER BY seq DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit], |row| {
            Ok(Event {
                at: row.get(0)?,
                commit: row.get(1)?,
                project: row.get(2)?,
                node: row.get(3)?,
                action: row.get(4)?,
                result: row.get(5)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}
