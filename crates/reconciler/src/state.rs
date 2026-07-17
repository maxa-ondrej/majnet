//! Event log — every action tagged with its causing commit (§12 principles).
//! The reconciler carries no state git doesn't; this is an audit trail, not
//! a source of truth.

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::Path;
use std::sync::Mutex;

/// Versioned schema migrations (ADR 0011), embedded from
/// `crates/reconciler/migrations` at compile time and run on startup.
mod embedded {
    refinery::embed_migrations!("migrations");
}

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
    /// Coarse activity type for the dashboard feed: `deploy` | `remove` |
    /// `config`. Set at write time so filtering never re-parses free text.
    pub kind: String,
}

/// The coarse activity type for an event, from its `action` verb. Kept in sync
/// with the `V5__event_kind.sql` backfill rule.
pub fn event_kind(action: &str) -> &'static str {
    match action.split_whitespace().next().unwrap_or("") {
        "converge" | "deploy" | "restart" | "promote" => "deploy",
        "gc" | "purge" | "purge-project" | "remove" => "remove",
        _ => "config",
    }
}

/// One env's build metadata as reported by the app's `/info` endpoint, recorded
/// at deploy time. `info` is whatever JSON the app returned (or `null`).
#[derive(Debug, serde::Serialize)]
pub struct AppInfo {
    pub class: String,
    pub commit: String,
    pub info: serde_json::Value,
    pub error: Option<String>,
    pub at: String,
}

/// One recorded terminal session (ADR 0016) — the audit row; the transcript
/// itself lives at `data_dir/transcripts/<id>.log`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TerminalSession {
    pub id: i64,
    pub actor: String,
    pub node: String,
    pub mode: String,
    pub target: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub bytes: Option<i64>,
}

impl Store {
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let mut conn = Connection::open(dir.join("reconciler.sqlite"))?;
        embedded::migrations::runner()
            .run(&mut conn)
            .context("running reconciler schema migrations")?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Operational key/value config (Discord webhook, alert thresholds, firing
    /// set). Not git-managed — same class as the bot's ghcr_token.
    pub fn get_config(&self, key: &str) -> Result<Option<String>> {
        use rusqlite::OptionalExtension;
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT value FROM config WHERE key = ?1", [key], |r| {
            r.get(0)
        })
        .optional()
        .context("reading config")
    }

    pub fn set_config(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO config (key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = ?2",
            rusqlite::params![key, value],
        )?;
        Ok(())
    }

    // ── Rename freeze (data-preserving rename) ─────────────────────────────

    /// Freeze a `(project, old_app, class)` rename: convergence + GC skip both
    /// the old and new names until it's completed.
    pub fn rename_add_pending(
        &self,
        project: &str,
        old_app: &str,
        new_app: &str,
        class: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO renames (project, old_app, new_app, class) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (project, old_app, class) DO UPDATE SET new_app = ?3",
            rusqlite::params![project, old_app, new_app, class],
        )?;
        Ok(())
    }

    /// The in-flight renames for a project+class as `(old_app, new_app)` pairs.
    pub fn renames_pending(&self, project: &str, class: &str) -> Result<Vec<(String, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT old_app, new_app FROM renames WHERE project = ?1 AND class = ?2")?;
        let rows = stmt
            .query_map([project, class], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Clear the freeze for one `(project, old_app, class)` — the migration is
    /// done, so normal convergence resumes (creates the new stack, GCs the old).
    pub fn rename_complete(&self, project: &str, old_app: &str, class: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM renames WHERE project = ?1 AND old_app = ?2 AND class = ?3",
            [project, old_app, class],
        )?;
        Ok(())
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
            "INSERT INTO events (commit_sha, project, node, action, result, kind) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![commit, project, node, action, result, event_kind(action)],
        )?;
        Ok(())
    }

    // ── App `/info` build metadata (scraped at deploy time) ────────────────

    /// Upsert the build metadata an app reported at `/info` for one env. `info`
    /// is the raw JSON the app returned (None when the probe found nothing);
    /// `error` records why it failed, if it did. Best-effort — a failure here
    /// must never fail a deploy.
    pub fn record_app_info(
        &self,
        project: &str,
        app: &str,
        class: &str,
        commit: &str,
        info: Option<&str>,
        error: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO app_info (project, app, class, commit_sha, info, error, at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, datetime('now'))
             ON CONFLICT (project, app, class) DO UPDATE
             SET commit_sha = ?4, info = ?5, error = ?6, at = datetime('now')",
            rusqlite::params![project, app, class, commit, info, error],
        )?;
        Ok(())
    }

    /// Every env's reported `/info` for an app, newest classes first is not
    /// meaningful — ordered by class name for a stable dashboard layout.
    pub fn app_info_for(&self, project: &str, app: &str) -> Result<Vec<AppInfo>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT class, commit_sha, info, error, at FROM app_info
             WHERE project = ?1 AND app = ?2 ORDER BY class",
        )?;
        let rows = stmt.query_map([project, app], |row| {
            let raw: Option<String> = row.get(2)?;
            // Stored as raw JSON text — re-parse so the API emits embedded JSON
            // rather than a quoted string. Unparseable/absent → null.
            let info = raw
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                .unwrap_or(serde_json::Value::Null);
            Ok(AppInfo {
                class: row.get(0)?,
                commit: row.get(1)?,
                info,
                error: row.get(3)?,
                at: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Drop `app_info` rows for a `(project, class)` whose app is not in `keep`
    /// (the class's rendered/kept app set) — so a GC'd, renamed, or archived app
    /// doesn't leave a stale build-info row behind. An empty `keep` clears the
    /// whole (project, class). Returns the number of rows removed.
    pub fn app_info_prune(&self, project: &str, class: &str, keep: &[String]) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let mut sql = String::from("DELETE FROM app_info WHERE project = ?1 AND class = ?2");
        if !keep.is_empty() {
            let placeholders = (0..keep.len())
                .map(|i| format!("?{}", i + 3))
                .collect::<Vec<_>>()
                .join(", ");
            sql.push_str(&format!(" AND app NOT IN ({placeholders})"));
        }
        let mut params: Vec<&dyn rusqlite::ToSql> = vec![&project, &class];
        params.extend(keep.iter().map(|k| k as &dyn rusqlite::ToSql));
        Ok(conn.execute(&sql, params.as_slice())?)
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

    /// Open a terminal session audit row (ADR 0016); returns its id, used as the
    /// transcript filename.
    pub fn terminal_open(&self, actor: &str, node: &str, mode: &str, target: &str) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO terminal_sessions (actor, node, mode, target) VALUES (?1, ?2, ?3, ?4)",
            [actor, node, mode, target],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Close a terminal session audit row, stamping the end time + transcript size.
    pub fn terminal_close(&self, id: i64, bytes: u64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE terminal_sessions SET ended_at = datetime('now'), bytes = ?2 WHERE id = ?1",
            rusqlite::params![id, bytes as i64],
        )?;
        Ok(())
    }

    /// Recent terminal sessions, newest first (for the dashboard audit view).
    pub fn terminal_sessions(&self, limit: u32) -> Result<Vec<TerminalSession>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, actor, node, mode, target, started_at, ended_at, bytes
             FROM terminal_sessions ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map([limit], |r| {
                Ok(TerminalSession {
                    id: r.get(0)?,
                    actor: r.get(1)?,
                    node: r.get(2)?,
                    mode: r.get(3)?,
                    target: r.get(4)?,
                    started_at: r.get(5)?,
                    ended_at: r.get(6)?,
                    bytes: r.get(7)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
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
            "SELECT at, commit_sha, project, node, action, result, kind FROM events ORDER BY seq DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit], |row| {
            Ok(Event {
                at: row.get(0)?,
                commit: row.get(1)?,
                project: row.get(2)?,
                node: row.get(3)?,
                action: row.get(4)?,
                result: row.get(5)?,
                kind: row
                    .get::<_, Option<String>>(6)?
                    .unwrap_or_else(|| "config".into()),
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
    fn event_kind_maps_actions() {
        assert_eq!(event_kind("converge poletime"), "deploy");
        assert_eq!(event_kind("restart api"), "deploy");
        assert_eq!(event_kind("gc projects-app-production-abc"), "remove");
        assert_eq!(event_kind("purge poletime"), "remove");
        assert_eq!(event_kind("purge-project"), "remove");
        assert_eq!(event_kind("rename api → web"), "config");
        assert_eq!(event_kind(""), "config");
    }

    #[test]
    fn recorded_event_carries_its_kind() {
        let s = store();
        s.record("c0ffee", "proj", "prod", "converge api", "deployed v1")
            .unwrap();
        s.record("c0ffee", "proj", "prod", "gc proj-api-x", "removed")
            .unwrap();
        let evs = s.recent(10).unwrap();
        assert_eq!(evs[0].kind, "remove"); // newest first
        assert_eq!(evs[1].kind, "deploy");
    }

    #[test]
    fn extending_untracked_app_fails() {
        let s = store();
        assert!(s.ephemeral_extend("proj", "ghost", 1).is_err());
    }

    #[test]
    fn app_info_upserts_and_parses_json() {
        let s = store();
        s.record_app_info(
            "proj",
            "api",
            "production",
            "c0ffee",
            Some(r#"{"version":"1.2.3"}"#),
            None,
        )
        .unwrap();
        // A later deploy of the same env replaces the row (upsert on PK).
        s.record_app_info(
            "proj",
            "api",
            "production",
            "beef",
            Some(r#"{"version":"1.3.0"}"#),
            None,
        )
        .unwrap();
        // A different env is tracked independently, and a failed probe records
        // its error with null info.
        s.record_app_info("proj", "api", "stable", "beef", None, Some("no /info"))
            .unwrap();

        let rows = s.app_info_for("proj", "api").unwrap();
        assert_eq!(rows.len(), 2);
        // Ordered by class: production before stable.
        assert_eq!(rows[0].class, "production");
        assert_eq!(rows[0].commit, "beef");
        assert_eq!(rows[0].info["version"], "1.3.0");
        assert!(rows[0].error.is_none());
        assert_eq!(rows[1].class, "stable");
        assert!(rows[1].info.is_null());
        assert_eq!(rows[1].error.as_deref(), Some("no /info"));

        // Unrelated app → nothing.
        assert!(s.app_info_for("proj", "other").unwrap().is_empty());
    }

    #[test]
    fn app_info_prune_drops_only_absent_apps_in_class() {
        let s = store();
        let info = Some(r#"{"version":"1"}"#);
        s.record_app_info("proj", "keep", "production", "c", info, None)
            .unwrap();
        s.record_app_info("proj", "gone", "production", "c", info, None)
            .unwrap();
        // Same-named app in another class must be untouched by a class-scoped prune.
        s.record_app_info("proj", "gone", "stable", "c", info, None)
            .unwrap();
        // Another project must be untouched too.
        s.record_app_info("other", "gone", "production", "c", info, None)
            .unwrap();

        let removed = s
            .app_info_prune("proj", "production", &["keep".into()])
            .unwrap();
        assert_eq!(removed, 1);
        assert!(!s.app_info_for("proj", "keep").unwrap().is_empty());
        // "gone" is pruned from production but survives in stable.
        let gone = s.app_info_for("proj", "gone").unwrap();
        assert_eq!(gone.len(), 1);
        assert_eq!(gone[0].class, "stable");
        // Other project untouched.
        assert_eq!(s.app_info_for("other", "gone").unwrap().len(), 1);

        // Empty keep-list clears the whole (project, class).
        assert_eq!(s.app_info_prune("proj", "stable", &[]).unwrap(), 1);
        assert!(s.app_info_for("proj", "gone").unwrap().is_empty());
    }
}
