//! Persistent state — SQLite. Deliberately minimal: the bot carries no state
//! git doesn't, except webhook delivery dedup and an audit log of actions.

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::Path;
use std::sync::Mutex;

/// Versioned schema migrations (ADR 0011), embedded from `crates/bot/migrations`
/// at compile time and run on startup by refinery.
mod embedded {
    refinery::embed_migrations!("migrations");
}

pub struct Store {
    conn: Mutex<Connection>,
}

/// A release as recorded in the store (ADR 0009), also the dashboard shape. A
/// release is a `vX.Y.Z`-tagged image publish; the migration lives in the ops
/// overlay, not here.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StoredRelease {
    pub app: String,
    pub version: String,
    pub commit: String,
    pub app_image: String,
    pub published_at: String,
}

/// Live status of an in-progress (or failed) app import (ADR 0010) — the
/// dashboard renders a skeleton + step progress from this until the app lands.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ImportStatus {
    pub app: String,
    /// `running` | `failed`.
    pub status: String,
    /// Current (or failed) step key: `snapshot` | `repo` | `commit` |
    /// `configure` | `secrets`.
    pub step: String,
    /// Human detail — the source repo while running, the error when failed.
    pub detail: String,
    pub updated_at: String,
}

impl Store {
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let mut conn = Connection::open(dir.join("bot.sqlite"))?;
        embedded::migrations::runner()
            .run(&mut conn)
            .context("running bot schema migrations")?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Runtime config value (ADR 0012), e.g. the GHCR pull token.
    pub fn get_config(&self, key: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        match conn.query_row("SELECT value FROM config WHERE key = ?1", [key], |row| {
            row.get::<_, String>(0)
        }) {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Set (or replace) a runtime config value.
    pub fn set_config(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO config (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [key, value],
        )?;
        Ok(())
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

    /// Record (or update) a release for `org/app` (ADR 0009). Keyed by version,
    /// so a re-published tag overwrites its digest rather than duplicating;
    /// `published_at` keeps its first-seen value (the ordering key).
    pub fn upsert_release(
        &self,
        org: &str,
        app: &str,
        version: &str,
        commit: &str,
        app_image: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO releases (org, app, version, commit_sha, app_image)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(org, app, version) DO UPDATE SET
                 commit_sha = excluded.commit_sha,
                 app_image = excluded.app_image",
            rusqlite::params![org, app, version, commit, app_image],
        )?;
        Ok(())
    }

    /// Start (or restart) an import: status running, step `snapshot`, and store
    /// the (secret-stripped) request JSON so a failed import can be retried.
    pub fn begin_import(&self, org: &str, app: &str, request: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO imports (org, app, status, step, detail, request)
             VALUES (?1, ?2, 'running', 'snapshot', '', ?3)
             ON CONFLICT(org, app) DO UPDATE SET
                 status = 'running', step = 'snapshot', detail = '',
                 request = excluded.request, updated_at = datetime('now')",
            rusqlite::params![org, app, request],
        )?;
        Ok(())
    }

    /// The stored (secret-stripped) request JSON for a retry, if any.
    pub fn import_request(&self, org: &str, app: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        match conn.query_row(
            "SELECT request FROM imports WHERE org = ?1 AND app = ?2",
            [org, app],
            |row| row.get::<_, String>(0),
        ) {
            Ok(s) => Ok(Some(s)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Upsert the live import status for `org/app` (ADR 0010).
    pub fn set_import(
        &self,
        org: &str,
        app: &str,
        status: &str,
        step: &str,
        detail: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO imports (org, app, status, step, detail)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(org, app) DO UPDATE SET
                 status = excluded.status,
                 step = excluded.step,
                 detail = excluded.detail,
                 updated_at = datetime('now')",
            rusqlite::params![org, app, status, step, detail],
        )?;
        Ok(())
    }

    /// Mark the import failed, keeping the step it reached (so the UI shows
    /// which step failed + the error).
    pub fn fail_import(&self, org: &str, app: &str, detail: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE imports SET status = 'failed', detail = ?3, updated_at = datetime('now')
             WHERE org = ?1 AND app = ?2",
            rusqlite::params![org, app, detail],
        )?;
        Ok(())
    }

    /// Drop the import record (on success — the real app now appears normally).
    pub fn clear_import(&self, org: &str, app: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM imports WHERE org = ?1 AND app = ?2",
            [org, app],
        )?;
        Ok(())
    }

    /// In-progress + failed imports for `org`, newest first.
    pub fn imports(&self, org: &str) -> Result<Vec<ImportStatus>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT app, status, step, detail, updated_at
             FROM imports WHERE org = ?1 ORDER BY updated_at DESC",
        )?;
        let rows = stmt
            .query_map([org], |row| {
                Ok(ImportStatus {
                    app: row.get(0)?,
                    status: row.get(1)?,
                    step: row.get(2)?,
                    detail: row.get(3)?,
                    updated_at: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Releases for `org/app`, newest first.
    pub fn releases(&self, org: &str, app: &str) -> Result<Vec<StoredRelease>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT app, version, commit_sha, app_image, published_at
             FROM releases WHERE org = ?1 AND app = ?2 ORDER BY published_at DESC, version DESC",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![org, app], |row| {
                Ok(StoredRelease {
                    app: row.get(0)?,
                    version: row.get(1)?,
                    commit: row.get(2)?,
                    app_image: row.get(3)?,
                    published_at: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The release version whose image is exactly `app_image` (digest-pinned),
    /// if one was recorded — used to label render-PR diffs with the version
    /// instead of a bare digest. `None` when no matching release is known.
    pub fn version_for_image(
        &self,
        org: &str,
        app: &str,
        app_image: &str,
    ) -> Result<Option<String>> {
        use rusqlite::OptionalExtension;
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT version FROM releases WHERE org = ?1 AND app = ?2 AND app_image = ?3
             ORDER BY published_at DESC LIMIT 1",
            rusqlite::params![org, app, app_image],
            |row| row.get(0),
        )
        .optional()
        .context("version_for_image")
    }
}
