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

/// A bot-side log event shaped like the reconciler's `/api/events` rows, so the
/// dashboard Activity feed can merge both streams. The bot has no node/commit,
/// so `action` = the logged kind, `result` = the detail, `project` = the org.
#[derive(Debug, serde::Serialize)]
pub struct ActivityEvent {
    pub at: String,
    pub commit: String,
    pub project: String,
    pub node: String,
    pub action: String,
    pub result: String,
    pub kind: String,
}

/// Coarse activity type for a bot event (the feed's `deploy | remove | config`).
/// The bot never deploys; deletions are removals, everything else is config.
pub fn event_kind(action: &str) -> &'static str {
    if action.contains("delete") {
        "remove"
    } else {
        "config"
    }
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
    /// The changelog this release was submitted with, if it came through the
    /// draft flow (`None` for releases recorded straight off a hand-cut tag).
    pub notes: Option<String>,
}

/// A pending draft release (ADR 0009 follow-up): the proposed next version and
/// its generated changelog, awaiting an operator's submit. Keyed per repo, so a
/// monorepo's single repo-wide draft covers every app that shares it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ReleaseDraft {
    pub repo: String,
    /// Proposed `vX.Y.Z` (with the leading `v`).
    pub version: String,
    pub bump: String,
    /// The tag the commit range was computed from (`""` = first release).
    pub base: String,
    pub commit_count: u32,
    pub notes: String,
    /// True once an operator edited the notes — a push refresh then keeps them.
    pub notes_edited: bool,
    pub updated_at: String,
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

/// Live progress of a release as it moves through the pipeline (ADR 0022) — the
/// dashboard renders a per-release stepper from this. Keyed by `(org, app,
/// version)`; `version` is bare (no leading `v`).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ReleaseProgress {
    pub app: String,
    pub version: String,
    /// `active` | `done` | `failed`.
    pub status: String,
    /// Current (or failed) stage: `committing` | `tagging` | `building` |
    /// `published` | `tracked`.
    pub stage: String,
    /// Human detail — the commit sha, the tag, the digest, or the error.
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

    /// Recent bot events for the dashboard Activity feed, newest first, shaped to
    /// merge with the reconciler's `/api/events`.
    pub fn recent_events(&self, limit: u32) -> Result<Vec<ActivityEvent>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT at, kind, org, detail FROM events ORDER BY seq DESC LIMIT ?1")?;
        let rows = stmt.query_map([limit], |row| {
            let at: String = row.get(0)?;
            let action: String = row.get(1)?;
            let org: Option<String> = row.get(2)?;
            let result: String = row.get(3)?;
            Ok(ActivityEvent {
                at,
                commit: String::new(),
                project: org.unwrap_or_default(),
                node: String::new(),
                kind: event_kind(&action).to_string(),
                action,
                result,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
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

    /// Advance a release's progress stage (ADR 0022). `status` is `done` once it
    /// reaches `tracked`, else `active`. Upsert keyed by `(org, app, version)`.
    /// Best-effort — callers ignore the error (progress is cosmetic).
    pub fn set_release_stage(
        &self,
        org: &str,
        app: &str,
        version: &str,
        stage: &str,
        detail: &str,
    ) -> Result<()> {
        let status = if stage == "tracked" { "done" } else { "active" };
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO release_progress (org, app, version, status, stage, detail)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(org, app, version) DO UPDATE SET
                 status = excluded.status,
                 stage = excluded.stage,
                 detail = excluded.detail,
                 updated_at = datetime('now')",
            rusqlite::params![org, app, version, status, stage, detail],
        )?;
        Ok(())
    }

    /// Mark a release's progress failed, keeping the stage it reached.
    pub fn fail_release_stage(
        &self,
        org: &str,
        app: &str,
        version: &str,
        detail: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE release_progress SET status = 'failed', detail = ?4,
                 updated_at = datetime('now')
             WHERE org = ?1 AND app = ?2 AND version = ?3",
            rusqlite::params![org, app, version, detail],
        )?;
        Ok(())
    }

    /// Release progress rows for `org`, newest first — active ones plus terminal
    /// (done/failed) ones from the last hour (older terminal rows are pruned on
    /// read, a lightweight TTL GC).
    pub fn release_progress(&self, org: &str) -> Result<Vec<ReleaseProgress>> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM release_progress
             WHERE status IN ('done', 'failed')
               AND updated_at < datetime('now', '-1 hour')",
            [],
        )?;
        let mut stmt = conn.prepare(
            "SELECT app, version, status, stage, detail, updated_at
             FROM release_progress WHERE org = ?1 ORDER BY updated_at DESC",
        )?;
        let rows = stmt
            .query_map([org], |row| {
                Ok(ReleaseProgress {
                    app: row.get(0)?,
                    version: row.get(1)?,
                    status: row.get(2)?,
                    stage: row.get(3)?,
                    detail: row.get(4)?,
                    updated_at: row.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Releases for `org/app`, newest first.
    pub fn releases(&self, org: &str, app: &str) -> Result<Vec<StoredRelease>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT r.app, r.version, r.commit_sha, r.app_image, r.published_at, n.notes
             FROM releases r
             LEFT JOIN release_notes n
               ON n.org = r.org AND n.app = r.app AND n.version = r.version
             WHERE r.org = ?1 AND r.app = ?2
             ORDER BY r.published_at DESC, r.version DESC",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![org, app], |row| {
                Ok(StoredRelease {
                    app: row.get(0)?,
                    version: row.get(1)?,
                    commit: row.get(2)?,
                    app_image: row.get(3)?,
                    published_at: row.get(4)?,
                    notes: row.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Delete a recorded release (and any changelog notes) — used by the registry
    /// reconcile (ADR 0009) to drop a version whose tag was removed from GHCR.
    /// Returns whether a row was deleted.
    pub fn delete_release(&self, org: &str, app: &str, version: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM release_notes WHERE org = ?1 AND app = ?2 AND version = ?3",
            rusqlite::params![org, app, version],
        )?;
        let n = conn.execute(
            "DELETE FROM releases WHERE org = ?1 AND app = ?2 AND version = ?3",
            rusqlite::params![org, app, version],
        )?;
        Ok(n > 0)
    }

    /// The pending draft release for a repo, if any.
    pub fn release_draft(&self, org: &str, repo: &str) -> Result<Option<ReleaseDraft>> {
        use rusqlite::OptionalExtension;
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT repo, version, bump, base, commit_count, notes, notes_edited, updated_at
             FROM release_drafts WHERE org = ?1 AND repo = ?2",
            rusqlite::params![org, repo],
            |row| {
                Ok(ReleaseDraft {
                    repo: row.get(0)?,
                    version: row.get(1)?,
                    bump: row.get(2)?,
                    base: row.get(3)?,
                    commit_count: row.get(4)?,
                    notes: row.get(5)?,
                    notes_edited: row.get::<_, i64>(6)? != 0,
                    updated_at: row.get(7)?,
                })
            },
        )
        .optional()
        .context("loading release draft")
    }

    /// Every pending draft across all orgs (org, draft), newest first — the
    /// fleet-wide set of release candidates for the top-bar "Releases" surface.
    pub fn all_release_drafts(&self) -> Result<Vec<(String, ReleaseDraft)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT org, repo, version, bump, base, commit_count, notes, notes_edited, updated_at
             FROM release_drafts ORDER BY updated_at DESC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    ReleaseDraft {
                        repo: row.get(1)?,
                        version: row.get(2)?,
                        bump: row.get(3)?,
                        base: row.get(4)?,
                        commit_count: row.get(5)?,
                        notes: row.get(6)?,
                        notes_edited: row.get::<_, i64>(7)? != 0,
                        updated_at: row.get(8)?,
                    },
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Prepare (or refresh) a repo's draft with freshly computed fields. The
    /// generated `notes` replace the stored ones only while the operator hasn't
    /// edited them — an edited draft keeps its notes but still tracks the latest
    /// version/count.
    pub fn upsert_release_draft(&self, org: &str, d: &ReleaseDraft) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO release_drafts (org, repo, version, bump, base, commit_count, notes)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(org, repo) DO UPDATE SET
                 version = excluded.version,
                 bump = excluded.bump,
                 base = excluded.base,
                 commit_count = excluded.commit_count,
                 notes = CASE WHEN release_drafts.notes_edited = 1
                              THEN release_drafts.notes ELSE excluded.notes END,
                 updated_at = datetime('now')",
            rusqlite::params![
                org,
                d.repo,
                d.version,
                d.bump,
                d.base,
                d.commit_count,
                d.notes
            ],
        )?;
        Ok(())
    }

    /// Replace a draft's notes with an operator's edit (marks it edited, so a
    /// later push refresh won't clobber them).
    pub fn set_release_draft_notes(&self, org: &str, repo: &str, notes: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE release_drafts SET notes = ?3, notes_edited = 1, updated_at = datetime('now')
             WHERE org = ?1 AND repo = ?2",
            rusqlite::params![org, repo, notes],
        )?;
        Ok(n == 1)
    }

    pub fn delete_release_draft(&self, org: &str, repo: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM release_drafts WHERE org = ?1 AND repo = ?2",
            rusqlite::params![org, repo],
        )?;
        Ok(())
    }

    /// Persist the changelog a submitted release carried, for one app.
    pub fn record_release_notes(
        &self,
        org: &str,
        app: &str,
        version: &str,
        notes: &str,
        submitted_by: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO release_notes (org, app, version, notes, submitted_by)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(org, app, version) DO UPDATE SET
                 notes = excluded.notes, submitted_by = excluded.submitted_by",
            rusqlite::params![org, app, version, notes, submitted_by],
        )?;
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        let dir = std::env::temp_dir().join(format!(
            "majnet-bot-state-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        Store::open(&dir).unwrap()
    }

    fn draft(repo: &str, version: &str, notes: &str) -> ReleaseDraft {
        ReleaseDraft {
            repo: repo.into(),
            version: version.into(),
            bump: "minor".into(),
            base: "v1.0.0".into(),
            commit_count: 3,
            notes: notes.into(),
            notes_edited: false,
            updated_at: String::new(),
        }
    }

    #[test]
    fn draft_upsert_edit_and_delete() {
        let s = store();
        assert!(s.release_draft("o", "api").unwrap().is_none());

        s.upsert_release_draft("o", &draft("api", "v1.1.0", "generated"))
            .unwrap();
        let d = s.release_draft("o", "api").unwrap().unwrap();
        assert_eq!(d.version, "v1.1.0");
        assert_eq!(d.notes, "generated");
        assert!(!d.notes_edited);

        // An operator edit sticks and marks the draft edited.
        assert!(s
            .set_release_draft_notes("o", "api", "hand-written")
            .unwrap());
        let d = s.release_draft("o", "api").unwrap().unwrap();
        assert_eq!(d.notes, "hand-written");
        assert!(d.notes_edited);

        // A push refresh updates version/count but must NOT clobber edited notes.
        let mut refreshed = draft("api", "v1.2.0", "regenerated");
        refreshed.commit_count = 7;
        s.upsert_release_draft("o", &refreshed).unwrap();
        let d = s.release_draft("o", "api").unwrap().unwrap();
        assert_eq!(d.version, "v1.2.0");
        assert_eq!(d.commit_count, 7);
        assert_eq!(d.notes, "hand-written", "operator notes survive a refresh");

        s.delete_release_draft("o", "api").unwrap();
        assert!(s.release_draft("o", "api").unwrap().is_none());
    }

    #[test]
    fn release_progress_advances_and_gcs() {
        let s = store();
        assert!(s.release_progress("o").unwrap().is_empty());

        // Advancing keeps status `active` until the terminal `tracked` stage.
        s.set_release_stage("o", "api", "1.2.0", "committing", "bump")
            .unwrap();
        s.set_release_stage("o", "api", "1.2.0", "building", "CI")
            .unwrap();
        let rows = s.release_progress("o").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].stage, "building");
        assert_eq!(rows[0].status, "active");
        assert_eq!(rows[0].version, "1.2.0");

        s.set_release_stage("o", "api", "1.2.0", "tracked", "stable")
            .unwrap();
        assert_eq!(s.release_progress("o").unwrap()[0].status, "done");

        // A failure keeps the stage it reached.
        s.set_release_stage("o", "web", "0.4.1", "tagging", "v0.4.1")
            .unwrap();
        s.fail_release_stage("o", "web", "0.4.1", "boom").unwrap();
        let web = s
            .release_progress("o")
            .unwrap()
            .into_iter()
            .find(|r| r.app == "web")
            .unwrap();
        assert_eq!(web.status, "failed");
        assert_eq!(web.stage, "tagging");
        assert_eq!(web.detail, "boom");

        // Terminal rows older than the TTL are pruned on read; active rows stay.
        {
            let conn = s.conn.lock().unwrap();
            conn.execute(
                "UPDATE release_progress SET updated_at = datetime('now', '-2 hours')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO release_progress (org, app, version, status, stage)
                 VALUES ('o', 'live', '9.9.9', 'active', 'building')",
                [],
            )
            .unwrap();
        }
        let rows = s.release_progress("o").unwrap();
        assert_eq!(rows.len(), 1, "stale done/failed pruned, active kept");
        assert_eq!(rows[0].app, "live");
    }

    #[test]
    fn all_release_drafts_spans_orgs() {
        let s = store();
        assert!(s.all_release_drafts().unwrap().is_empty());
        s.upsert_release_draft("org-a", &draft("api", "v1.0.0", "n"))
            .unwrap();
        s.upsert_release_draft("org-b", &draft("web", "v2.1.0", "n"))
            .unwrap();
        let all = s.all_release_drafts().unwrap();
        assert_eq!(all.len(), 2);
        assert!(all
            .iter()
            .any(|(org, d)| org == "org-a" && d.repo == "api" && d.version == "v1.0.0"));
        assert!(all.iter().any(|(org, d)| org == "org-b" && d.repo == "web"));
        // Cleared drafts drop out of the fleet-wide list.
        s.delete_release_draft("org-a", "api").unwrap();
        assert_eq!(s.all_release_drafts().unwrap().len(), 1);
    }

    #[test]
    fn release_notes_attach_to_the_listed_release() {
        let s = store();
        s.upsert_release("o", "api", "v1.1.0", "abc123", "ghcr.io/o/api@sha256:d")
            .unwrap();
        // No notes yet.
        assert!(s.releases("o", "api").unwrap()[0].notes.is_none());
        // Submitting a draft persists notes for the version; the list picks them up.
        s.record_release_notes("o", "api", "v1.1.0", "## Features\n- x", "alice")
            .unwrap();
        let r = &s.releases("o", "api").unwrap()[0];
        assert_eq!(r.version, "v1.1.0");
        assert_eq!(r.notes.as_deref(), Some("## Features\n- x"));
    }
}
