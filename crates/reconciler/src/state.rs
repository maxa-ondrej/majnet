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
             );
             CREATE TABLE IF NOT EXISTS data_migrations (
                 project TEXT NOT NULL,
                 app TEXT NOT NULL,
                 class TEXT NOT NULL,
                 done_at TEXT NOT NULL DEFAULT (datetime('now')),
                 PRIMARY KEY (project, app, class)
             );",
        )?;
        // Phase-5 dashboard: TTL extension. Idempotent poor-man's migration.
        let _ = conn.execute(
            "ALTER TABLE ephemeral_stacks ADD COLUMN extended_until TEXT",
            [],
        );
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

    // ── Data migration idempotency (ADR 0010 phase 3) ─────────────────────

    /// True if a data restore already completed for this stack — the guard that
    /// keeps a re-upload from restoring twice into a live DB.
    pub fn data_migration_done(&self, project: &str, app: &str, class: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM data_migrations WHERE project = ?1 AND app = ?2 AND class = ?3",
            [project, app, class],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Record a completed data restore (only after success, so a failed restore
    /// stays retryable).
    pub fn record_data_migration(&self, project: &str, app: &str, class: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO data_migrations (project, app, class) VALUES (?1, ?2, ?3)",
            [project, app, class],
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
    /// A dashboard extension (`extended_until`) postpones it.
    pub fn ephemeral_ttl_expired(&self, project: &str, app: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let expired = conn
            .query_row(
                "SELECT 1 FROM ephemeral_stacks
                 WHERE project = ?1 AND app = ?2 AND first_deployed < datetime('now', '-7 days')
                   AND (extended_until IS NULL OR extended_until < datetime('now'))",
                [project, app],
                |_| Ok(()),
            )
            .is_ok();
        Ok(expired)
    }

    /// Apps whose 48 h post-close grace has run out (extensions postpone).
    pub fn ephemeral_grace_expired(&self, project: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT app FROM ephemeral_stacks
             WHERE project = ?1 AND missing_since < datetime('now', '-48 hours')
               AND (extended_until IS NULL OR extended_until < datetime('now'))",
        )?;
        let apps = stmt
            .query_map([project], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        Ok(apps)
    }

    /// Postpone GC (TTL and grace) for a tracked preview by `days` from now.
    /// Returns the new deadline; errors if the app isn't tracked.
    pub fn ephemeral_extend(&self, project: &str, app: &str, days: u32) -> Result<String> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE ephemeral_stacks SET extended_until = datetime('now', ?3)
             WHERE project = ?1 AND app = ?2",
            rusqlite::params![project, app, format!("+{days} days")],
        )?;
        anyhow::ensure!(
            changed == 1,
            "{project}/{app} is not a tracked ephemeral stack"
        );
        Ok(conn.query_row(
            "SELECT extended_until FROM ephemeral_stacks WHERE project = ?1 AND app = ?2",
            [project, app],
            |row| row.get(0),
        )?)
    }

    pub fn ephemeral_forget(&self, project: &str, app: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM ephemeral_stacks WHERE project = ?1 AND app = ?2",
            [project, app],
        )?;
        Ok(())
    }

    #[cfg(test)]
    fn raw(&self, sql: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(sql, [])?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        let dir = std::env::temp_dir().join(format!(
            "majnet-state-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        Store::open(&dir).unwrap()
    }

    #[test]
    fn extension_postpones_ttl_and_grace() {
        let s = store();
        s.ephemeral_mark_seen("proj", "app-pr1").unwrap();
        // Age the stack past both deadlines.
        s.raw(
            "UPDATE ephemeral_stacks SET
                 first_deployed = datetime('now', '-8 days'),
                 missing_since = datetime('now', '-3 days')",
        )
        .unwrap();
        assert!(s.ephemeral_ttl_expired("proj", "app-pr1").unwrap());
        assert_eq!(s.ephemeral_grace_expired("proj").unwrap(), ["app-pr1"]);

        // An extension postpones both.
        let until = s.ephemeral_extend("proj", "app-pr1", 2).unwrap();
        assert!(!until.is_empty());
        assert!(!s.ephemeral_ttl_expired("proj", "app-pr1").unwrap());
        assert!(s.ephemeral_grace_expired("proj").unwrap().is_empty());

        // A lapsed extension stops protecting.
        s.raw("UPDATE ephemeral_stacks SET extended_until = datetime('now', '-1 hour')")
            .unwrap();
        assert!(s.ephemeral_ttl_expired("proj", "app-pr1").unwrap());
    }

    #[test]
    fn extending_untracked_app_fails() {
        let s = store();
        assert!(s.ephemeral_extend("proj", "ghost", 1).is_err());
    }
}
