//! App migration from an external PaaS (ADR 0010).
//!
//! Phase 1 — **repo + CI import**: seed a new app's source repo from an old
//! GitHub repo by snapshotting its default-branch **tarball** and writing it as
//! one commit via the git-data API (blobs are base64-encoded, so binaries
//! survive), alongside the MajNet CI workflows from the chosen template. Then
//! scaffold the manifest + declare the app in `project.yaml`.
//!
//! (GitHub's server-side source-import API — the obvious "copy a repo" path —
//! was deprecated, so we snapshot instead: current tree only, no history, which
//! is fine since the old repo keeps its history.)
//!
//! Runs as a background task off `apps_post`; progress is logged to the events
//! feed. The optional read token for a private source is held only in memory
//! here — never persisted, never committed to `project.yaml`.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;

use crate::dashboard_api::NewApp;
use crate::AppState;

/// Where an imported app's source comes from (New-app "Import existing" form).
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct ImportSource {
    /// Old repo, e.g. `https://github.com/old-org/blog`.
    pub repo: String,
    /// Optional read token for a private source (a GitHub PAT). In memory only.
    #[serde(default)]
    pub token: Option<String>,
    /// The old app's environment variables (dotenv `KEY=VALUE` lines) to import
    /// as SOPS-encrypted secrets (ADR 0010 phase 2). In memory only.
    #[serde(default)]
    pub env: Option<String>,
}

/// Import an app: snapshot the source repo + CI into a new repo, then scaffold.
pub async fn import_app(
    state: &AppState,
    org: &str,
    req: &NewApp,
    actor: &str,
    source: &ImportSource,
) -> Result<()> {
    let app = req.name.as_str();
    let client = state.github.org_client(org).await?;
    state.store.log_event(
        "app-import",
        Some(org),
        &format!("{app} <- {}", source.repo),
    )?;

    // Record a secret-stripped copy of the request so a failed import is
    // retryable — the token + env carry secrets and must never hit disk.
    let mut stored = req.clone();
    if let Some(imp) = stored.import.as_mut() {
        imp.token = None;
        imp.env = None;
    }
    state
        .store
        .begin_import(org, app, &serde_json::to_string(&stored)?)?;

    // Snapshot the source tree, add the MajNet CI workflows, and write it all as
    // one commit onto the new repo's `main`.
    let mut files = fetch_repo_snapshot(state, source).await?;
    anyhow::ensure!(!files.is_empty(), "source repo snapshot is empty");
    let platform = crate::dashboard_api::read_platform(state).await?;
    for (path, content) in ci_workflow_files(&platform, &req.template, org, app)? {
        files.insert(path, content.into_bytes());
    }

    state
        .store
        .set_import(org, app, "running", "repo", &source.repo)?;
    ensure_repo(&client, org, app).await?;
    state
        .store
        .set_import(org, app, "running", "commit", &source.repo)?;
    commit_snapshot(
        &client,
        org,
        app,
        &files,
        &format!("chore: import {} + MajNet CI", source.repo),
    )
    .await?;
    crate::org_sync::protect_app_main(&client, org, app).await?;

    // The repo now exists → declaring it in project.yaml won't re-scaffold from
    // the template (org-sync skips existing repos).
    state
        .store
        .set_import(org, app, "running", "configure", &source.repo)?;
    crate::dashboard_api::scaffold_and_declare(state, org, req, actor, true).await?;

    // Phase 2: import env vars as SOPS-encrypted secrets for the target class.
    if let Some(env_text) = source.env.as_deref().filter(|s| !s.trim().is_empty()) {
        state
            .store
            .set_import(org, app, "running", "secrets", &source.repo)?;
        import_secrets(state, org, req, env_text).await?;
    }

    state.store.clear_import(org, app)?;
    state.store.log_event(
        "app-import-done",
        Some(org),
        &format!("{app} imported from {}", source.repo),
    )?;
    tracing::info!(org, app, "app import complete");
    Ok(())
}

/// Encrypt the old app's env vars into `secrets.<class>.yaml` for the target
/// class and declare the keys in that class overlay (ADR 0010 phase 2). Secrets
/// are delivered as tmpfs files, never env vars (§14) — a migrated app reads
/// them from its secrets dir. Encryption uses the ops `.sops.yaml` recipients,
/// exactly as an operator running `sops apps/<app>/secrets.<class>.yaml` would.
async fn import_secrets(state: &AppState, org: &str, req: &NewApp, env_text: &str) -> Result<()> {
    let file = format!("{}.yaml", target_class(&req.classes));
    set_app_secrets(state, org, &req.name, &file, env_text).await?;
    Ok(())
}

/// Encode a dotenv blob into the inline `secrets:` map of a manifest FILE
/// (`base.yaml` or `<class>.yaml`) — the shared path behind app-import (ADR 0010
/// phase 2) and the dashboard "set secrets" action. Each value is an `age`-encrypted
/// `majnet:` envelope; a class overlay encrypts to that class's recipient, base.yaml
/// to every recipient (a base secret is inherited by all classes, ADR 0024).
/// Returns the number of secrets written. **Replaces** the file's whole secret set
/// (the bot can't read existing values to merge); an empty set clears them. Secrets
/// are delivered to apps as tmpfs files, never env vars (§14).
pub(crate) async fn set_app_secrets(
    state: &AppState,
    org: &str,
    app: &str,
    file: &str,
    env_text: &str,
) -> Result<usize> {
    // Only keys that are valid bare secret filenames (§14) — skip + warn on the
    // rest so a stray key can't break the render.
    let mut secrets = BTreeMap::new();
    for (k, v) in parse_dotenv(env_text) {
        if valid_secret_name(&k) {
            secrets.insert(k, v);
        } else {
            tracing::warn!(
                org,
                app,
                key = k,
                "skipping env var — not a valid secret name"
            );
        }
    }
    // Encrypt each value inline to the file's recipient(s) (ADR 0024) — one
    // `majnet:<base64(age ciphertext)>` line per secret. The bot holds only the
    // public recipient(s), so it can encode but never decode. An empty set is a
    // valid "clear all" (skip encryption; the file's `secrets` key is removed).
    let mut inline = BTreeMap::new();
    if !secrets.is_empty() {
        let recipients = state.config.age_recipients_for_file(file);
        anyhow::ensure!(
            !recipients.is_empty(),
            "no age recipient configured for {file} — set MAJNET_AGE_PRODUCTION_RECIPIENT / MAJNET_AGE_STABLE_RECIPIENT"
        );
        for (k, v) in &secrets {
            inline.insert(
                k.clone(),
                age_encrypt_inline(&recipients, v)
                    .await
                    .with_context(|| format!("encrypting secret '{k}'"))?,
            );
        }
    }

    // Write the inline `secrets:` map into the file, replacing its whole set. Inline
    // is authoritative at deploy, so any legacy SOPS file is ignored (cleaned up by
    // the phase-3 migration).
    write_inline_secrets_overlay(state, org, app, file, &inline).await?;

    state.store.log_event(
        "app-secrets-set",
        Some(org),
        &format!("{app}: {} secrets → {file}", secrets.len()),
    )?;
    tracing::info!(
        org,
        app,
        file,
        count = secrets.len(),
        "set app secrets (inline)"
    );
    Ok(secrets.len())
}

/// Encrypt a plaintext secret value to one or more age `recipients` as a single-line
/// `majnet:<base64(age ciphertext)>` envelope (ADR 0024). Binary `age` output (no
/// armor) base64-encoded, so the whole value fits on one YAML line. Multiple
/// recipients (base.yaml) → any of their keys can decrypt. Only public recipients
/// are needed — the bot cannot decrypt.
async fn age_encrypt_inline(recipients: &[&str], plaintext: &str) -> Result<String> {
    use base64::Engine;
    use tokio::io::AsyncWriteExt;
    let mut cmd = tokio::process::Command::new("age");
    for r in recipients {
        cmd.arg("-r").arg(r);
    }
    let mut child = cmd
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("spawning age (is it installed?)")?;
    child
        .stdin
        .take()
        .context("no stdin")?
        .write_all(plaintext.as_bytes())
        .await?;
    let out = child.wait_with_output().await?;
    if !out.status.success() {
        bail!(
            "age encrypt failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(format!(
        "{}{}",
        majnet_common::manifest::SECRET_ENVELOPE_PREFIX,
        base64::engine::general_purpose::STANDARD.encode(&out.stdout)
    ))
}

/// Replace a manifest FILE's `secrets:` with the given inline `KEY → majnet:…` map
/// (ADR 0024), preserving every other key in the file. `file` is `base.yaml` (shared
/// across classes) or `<class>.yaml` (that class only).
async fn write_inline_secrets_overlay(
    state: &AppState,
    org: &str,
    app: &str,
    file: &str,
    inline: &BTreeMap<String, String>,
) -> Result<()> {
    let path = format!("apps/{app}/{file}");
    let client = state.github.org_client(org).await?;
    let repos = client.repos(org, "ops");
    let current = crate::promote::read_file(&repos, &path)
        .await?
        .map(|(c, _)| c)
        .unwrap_or_else(|| "{}\n".to_string());

    let mut overlay: serde_yaml::Value =
        serde_yaml::from_str(&current).context("parsing manifest file")?;
    if !overlay.is_mapping() {
        overlay = serde_yaml::Value::Mapping(Default::default());
    }
    let map = overlay.as_mapping_mut().unwrap();
    if inline.is_empty() {
        // Clearing: drop the key entirely rather than leaving `secrets: {}`.
        map.remove("secrets");
    } else {
        let mut secrets = serde_yaml::Mapping::new();
        for (k, v) in inline {
            secrets.insert(k.clone().into(), v.clone().into());
        }
        map.insert("secrets".into(), serde_yaml::Value::Mapping(secrets));
    }

    let yaml = serde_yaml::to_string(&overlay)?;
    let msg = if inline.is_empty() {
        format!("secrets({app}): clear all in {file}")
    } else {
        format!("secrets({app}): set {} value(s) in {file}", inline.len())
    };
    crate::dashboard_api::commit_file(state, org, &path, &yaml, &msg).await
}

/// Migration target class: the running app is production, so prefer it, then the
/// most-stable selected class.
fn target_class(classes: &[String]) -> &str {
    for pref in ["production", "stable", "testing", "ephemeral"] {
        if classes.iter().any(|c| c == pref) {
            return pref;
        }
    }
    classes.first().map(String::as_str).unwrap_or("production")
}

/// Parse dotenv `KEY=VALUE` text: skips blanks/comments, strips a leading
/// `export`, and unwraps matching surrounding quotes.
fn parse_dotenv(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let k = k.trim();
        if k.is_empty() {
            continue;
        }
        let mut v = v.trim();
        if v.len() >= 2
            && ((v.starts_with('"') && v.ends_with('"'))
                || (v.starts_with('\'') && v.ends_with('\'')))
        {
            v = &v[1..v.len() - 1];
        }
        out.insert(k.to_string(), v.to_string());
    }
    out
}

/// A valid bare secret file name (§14): non-empty, no path separators.
fn valid_secret_name(name: &str) -> bool {
    !name.is_empty() && !name.contains('/') && !name.contains("..")
}

/// Create the destination repo (empty), tolerating "already exists" so a retry
/// after a partial import reuses it. If it already exists, ensure it's not
/// archived — a prior failed import can leave it undeclared, and org-sync
/// archives undeclared repos (making it read-only for the retry).
async fn ensure_repo(client: &octocrab::Octocrab, org: &str, app: &str) -> Result<()> {
    // App repos merge PRs by squash only.
    let squash_only = json!({
        "allow_squash_merge": true,
        "allow_merge_commit": false,
        "allow_rebase_merge": false,
    });
    match client
        .post(
            format!("/orgs/{org}/repos"),
            Some(&json!({
                "name": app,
                "private": true,
                "auto_init": false,
                "allow_squash_merge": true,
                "allow_merge_commit": false,
                "allow_rebase_merge": false,
            })),
        )
        .await
    {
        Ok::<serde_json::Value, _>(_) => Ok(()),
        Err(octocrab::Error::GitHub { source, .. }) if source.status_code == 422 => {
            // Exists — ensure it's not archived and enforce squash-only merges.
            let mut patch = squash_only;
            patch["archived"] = json!(false);
            let _: serde_json::Value = client
                .patch(format!("/repos/{org}/{app}"), Some(&patch))
                .await
                .with_context(|| format!("reconciling existing repo {app}"))?;
            Ok(())
        }
        Err(e) => Err(e).with_context(|| format!("creating repo {app}")),
    }
}

/// Snapshot the source repo's default branch as `path → bytes` (no history).
/// Public source: unauthenticated; private: the in-memory read token.
async fn fetch_repo_snapshot(
    state: &AppState,
    source: &ImportSource,
) -> Result<BTreeMap<String, Vec<u8>>> {
    let (owner, repo) = parse_github_slug(&source.repo)?;
    let url = format!("https://api.github.com/repos/{owner}/{repo}/tarball");
    let mut req = state
        .http
        .get(&url)
        .header(reqwest::header::USER_AGENT, "majnet-bot")
        .header(reqwest::header::ACCEPT, "application/vnd.github+json");
    if let Some(token) = &source.token {
        req = req.bearer_auth(token);
    }
    let bytes = req
        .send()
        .await?
        .error_for_status()
        .with_context(|| format!("fetching tarball for {owner}/{repo}"))?
        .bytes()
        .await?;
    majnet_common::tarball::untar(&bytes).context("unpacking source tarball")
}

/// Write `files` as a single commit on the new repo's `main`, then force-update
/// the ref. A zero-commit repo has no writable git object DB, so bootstrap
/// `main` via the Contents API first (same fix as `ensure_ops_repo`); the
/// snapshot commit then replaces that placeholder tree.
async fn commit_snapshot(
    client: &octocrab::Octocrab,
    org: &str,
    app: &str,
    files: &BTreeMap<String, Vec<u8>>,
    message: &str,
) -> Result<()> {
    let repo = format!("/repos/{org}/{app}");
    if crate::git::get_branch_head(client, &repo, "main")
        .await?
        .is_none()
    {
        client
            .repos(org, app)
            .create_file(
                ".majnet-init",
                "chore: initialize repo",
                "placeholder — replaced by the import commit\n",
            )
            .send()
            .await
            .context("initializing empty repo via the Contents API")?;
    }
    // The `main` ref can lag briefly after `create_file` — poll for it.
    let mut head = None;
    for _ in 0..10 {
        if let Some(h) = crate::git::get_branch_head(client, &repo, "main").await? {
            head = Some(h);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    let head = head.context("repo has no main branch after initialization")?;

    let mut blobs = BTreeMap::new();
    for (path, content) in files {
        let sha = crate::git::create_blob(client, &repo, content)
            .await
            .with_context(|| format!("blob for {path}"))?;
        blobs.insert(path.clone(), sha);
    }
    let tree = crate::git::create_tree_from_blobs(client, &repo, &blobs).await?;
    let commit = crate::git::create_commit(client, &repo, &tree, &[&head], message).await?;
    crate::git::force_update_ref(client, &repo, "main", &commit).await
}

/// `owner`, `repo` from a GitHub URL (`https://github.com/owner/repo[.git]`).
fn parse_github_slug(url: &str) -> Result<(String, String)> {
    let s = url.trim().trim_end_matches('/');
    let s = s.strip_suffix(".git").unwrap_or(s);
    let s = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .unwrap_or(s);
    let s = s
        .strip_prefix("github.com/")
        .or_else(|| s.strip_prefix("github.com:"))
        .unwrap_or(s);
    let mut it = s.split('/');
    let owner = it
        .next()
        .filter(|x| !x.is_empty())
        .context("repo URL has no owner")?;
    let repo = it
        .next()
        .filter(|x| !x.is_empty())
        .context("repo URL has no repo")?;
    Ok((owner.to_string(), repo.to_string()))
}

/// The template's `.github/workflows/*` as destination-path → content, with
/// `{{app}}`/`{{org}}` placeholders substituted (same as `create_repo_from_template`).
fn ci_workflow_files(
    platform: &BTreeMap<String, String>,
    template: &str,
    org: &str,
    app: &str,
) -> Result<BTreeMap<String, String>> {
    let prefix = format!("repo-templates/{template}/.github/workflows/");
    let files: BTreeMap<String, String> = platform
        .iter()
        .filter_map(|(path, content)| {
            let rel = path.strip_prefix(&prefix)?;
            Some((
                format!(".github/workflows/{rel}"),
                content.replace("{{app}}", app).replace("{{org}}", org),
            ))
        })
        .collect();
    ensure_ci(&files, template)?;
    Ok(files)
}

fn ensure_ci(files: &BTreeMap<String, String>, template: &str) -> Result<()> {
    anyhow::ensure!(
        !files.is_empty(),
        "template '{template}' has no .github/workflows to inject"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ci_workflow_files, parse_github_slug, ImportSource};
    use std::collections::BTreeMap;

    #[test]
    fn parses_github_slugs() {
        for url in [
            "https://github.com/maxa-ondrej/space-alert",
            "https://github.com/maxa-ondrej/space-alert/",
            "https://github.com/maxa-ondrej/space-alert.git",
            "github.com/maxa-ondrej/space-alert",
        ] {
            assert_eq!(
                parse_github_slug(url).unwrap(),
                ("maxa-ondrej".to_string(), "space-alert".to_string()),
                "{url}"
            );
        }
        assert!(parse_github_slug("https://github.com/").is_err());
    }

    #[test]
    fn import_source_token_optional() {
        let s: ImportSource = serde_json::from_str(r#"{"repo":"https://github.com/o/a"}"#).unwrap();
        assert!(s.token.is_none());
        let s: ImportSource =
            serde_json::from_str(r#"{"repo":"https://github.com/o/a","token":"ghp_x"}"#).unwrap();
        assert_eq!(s.token.as_deref(), Some("ghp_x"));
    }

    #[test]
    fn ci_files_are_scoped_to_the_template_and_substituted() {
        let platform = BTreeMap::from([
            (
                "repo-templates/web-app/.github/workflows/build.yaml".to_string(),
                "image: ghcr.io/{{org}}/{{app}}\n".to_string(),
            ),
            (
                "repo-templates/web-app/.github/workflows/release.yaml".to_string(),
                "name: release\n".to_string(),
            ),
            // A different template + a non-workflow file must be excluded.
            (
                "repo-templates/rust-service/.github/workflows/build.yaml".to_string(),
                "x\n".to_string(),
            ),
            (
                "repo-templates/web-app/Dockerfile".to_string(),
                "FROM x\n".to_string(),
            ),
        ]);
        let out = ci_workflow_files(&platform, "web-app", "acme", "blog").unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(
            out.get(".github/workflows/build.yaml").unwrap(),
            "image: ghcr.io/acme/blog\n"
        );
        assert!(out.contains_key(".github/workflows/release.yaml"));
    }

    #[test]
    fn missing_template_workflows_errors() {
        let platform = BTreeMap::new();
        assert!(ci_workflow_files(&platform, "web-app", "o", "a").is_err());
    }

    #[test]
    fn dotenv_parsing_handles_comments_export_and_quotes() {
        let env = super::parse_dotenv(
            "# comment\n\
             export FOO=bar\n\
             BAZ=\"quoted value\"\n\
             QUX='single'\n\
             EMPTY=\n\
             \n\
             =novalue\n\
             URL=postgres://u:p@h/db\n",
        );
        assert_eq!(env.get("FOO").unwrap(), "bar");
        assert_eq!(env.get("BAZ").unwrap(), "quoted value");
        assert_eq!(env.get("QUX").unwrap(), "single");
        assert_eq!(env.get("EMPTY").unwrap(), "");
        assert_eq!(env.get("URL").unwrap(), "postgres://u:p@h/db");
        assert!(!env.contains_key("")); // the `=novalue` line is dropped
        assert_eq!(env.len(), 5);
    }

    #[test]
    fn target_class_prefers_production_then_stability() {
        let c = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            super::target_class(&c(&["testing", "production"])),
            "production"
        );
        assert_eq!(super::target_class(&c(&["ephemeral", "stable"])), "stable");
        assert_eq!(super::target_class(&c(&["testing"])), "testing");
        assert_eq!(super::target_class(&[]), "production");
    }

    #[test]
    fn secret_names_reject_paths() {
        assert!(super::valid_secret_name("DATABASE_URL"));
        assert!(!super::valid_secret_name("../etc/passwd"));
        assert!(!super::valid_secret_name("a/b"));
        assert!(!super::valid_secret_name(""));
    }
}
