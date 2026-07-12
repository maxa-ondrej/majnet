//! App migration from an external PaaS (ADR 0010).
//!
//! Phase 1 — **repo + CI import**: seed a new app's source repo from an old
//! GitHub repo via the GitHub **source-import API** (server-side, so it carries
//! full history + binaries the git-data blob path can't), normalize the default
//! branch to `main`, inject the MajNet CI workflows from the chosen template,
//! then scaffold the manifest + declare the app in `project.yaml`.
//!
//! Slow (source-import is async), so this runs as a background task off
//! `apps_post`; progress is logged to the events feed. The optional read token
//! for a private source is held only in memory here — never persisted, never
//! committed to `project.yaml`.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;
use std::time::Duration;

use crate::dashboard_api::NewApp;
use crate::AppState;

/// Where an imported app's source comes from (New-app "Import existing" form).
#[derive(Debug, Clone, Deserialize)]
pub struct ImportSource {
    /// Old repo, e.g. `https://github.com/old-org/blog`.
    pub repo: String,
    /// Optional read token for a private source (a GitHub PAT). In memory only.
    #[serde(default)]
    pub token: Option<String>,
}

const IMPORT_POLL: Duration = Duration::from_secs(5);
const IMPORT_ATTEMPTS: u32 = 180; // ~15 min ceiling

/// Import an app: create + seed the source repo, inject CI, then scaffold.
pub async fn import_app(
    state: &AppState,
    org: &str,
    req: &NewApp,
    actor: &str,
    source: &ImportSource,
) -> Result<()> {
    let app = req.name.as_str();
    let client = state.github.org_client(org).await?;
    state
        .store
        .log_event("app-import", Some(org), &format!("{app} ← {}", source.repo))?;

    create_empty_repo(&client, org, app).await?;
    run_source_import(&client, org, app, source).await?;
    normalize_default_branch(&client, org, app).await?;

    let platform = crate::dashboard_api::read_platform(state).await?;
    let workflows = ci_workflow_files(&platform, &req.template, org, app)?;
    inject_files(&client, org, app, &workflows, "chore: add MajNet CI (build + release)").await?;

    crate::org_sync::protect_app_main(&client, org, app).await?;

    // The repo now exists → declaring it in project.yaml won't re-scaffold from
    // the template (org-sync skips existing repos).
    crate::dashboard_api::scaffold_and_declare(state, org, req, actor).await?;

    state.store.log_event(
        "app-import-done",
        Some(org),
        &format!("{app} imported from {}", source.repo),
    )?;
    tracing::info!(org, app, "app import complete");
    Ok(())
}

/// The empty destination repo the source-import writes into.
async fn create_empty_repo(client: &octocrab::Octocrab, org: &str, app: &str) -> Result<()> {
    let _: serde_json::Value = client
        .post(
            format!("/orgs/{org}/repos"),
            Some(&json!({ "name": app, "private": true, "auto_init": false })),
        )
        .await
        .with_context(|| format!("creating repo {app}"))?;
    Ok(())
}

/// Start the GitHub source-import from `source.repo` and poll to completion.
async fn run_source_import(
    client: &octocrab::Octocrab,
    org: &str,
    app: &str,
    source: &ImportSource,
) -> Result<()> {
    let mut body = json!({ "vcs": "git", "vcs_url": source.repo });
    if let Some(token) = &source.token {
        // For a GitHub source, any username + the PAT as the password works.
        body["vcs_username"] = json!("x-access-token");
        body["vcs_password"] = json!(token);
    }
    let _: serde_json::Value = client
        .put(format!("/repos/{org}/{app}/import"), Some(&body))
        .await
        .context("starting source import")?;

    for _ in 0..IMPORT_ATTEMPTS {
        tokio::time::sleep(IMPORT_POLL).await;
        let status: serde_json::Value = client
            .get(format!("/repos/{org}/{app}/import"), None::<&()>)
            .await
            .context("polling import status")?;
        match status["status"].as_str().unwrap_or_default() {
            "complete" => return Ok(()),
            "error" | "detection_failed" | "auth_failed" => bail!(
                "source import failed: {}",
                status["status_text"].as_str().unwrap_or("unknown")
            ),
            other => tracing::info!(org, app, status = other, "source import in progress"),
        }
    }
    bail!("source import did not complete within the timeout");
}

/// MajNet assumes `main`; rename an imported `master`/other default to it.
async fn normalize_default_branch(client: &octocrab::Octocrab, org: &str, app: &str) -> Result<()> {
    let info: serde_json::Value = client
        .get(format!("/repos/{org}/{app}"), None::<&()>)
        .await
        .context("reading imported repo")?;
    let default = info["default_branch"].as_str().unwrap_or("main");
    if default != "main" {
        let _: serde_json::Value = client
            .post(
                format!("/repos/{org}/{app}/branches/{default}/rename"),
                Some(&json!({ "new_name": "main" })),
            )
            .await
            .with_context(|| format!("renaming default branch {default} → main"))?;
    }
    Ok(())
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

/// Commit a set of files onto the repo's `main` head (one commit).
async fn inject_files(
    client: &octocrab::Octocrab,
    org: &str,
    app: &str,
    files: &BTreeMap<String, String>,
    message: &str,
) -> Result<()> {
    let repo = format!("/repos/{org}/{app}");
    let head = crate::git::get_branch_head(client, &repo, "main")
        .await?
        .context("imported repo has no main branch")?;
    let base_tree = crate::git::commit_tree(client, &repo, &head).await?;
    let changes: BTreeMap<String, Option<String>> = files
        .iter()
        .map(|(p, c)| (p.clone(), Some(c.clone())))
        .collect();
    let tree = crate::git::create_tree_incremental(client, &repo, &base_tree, &changes).await?;
    let commit = crate::git::create_commit(client, &repo, &tree, &[&head], message).await?;
    crate::git::force_update_ref(client, &repo, "main", &commit).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ci_workflow_files, ImportSource};
    use std::collections::BTreeMap;

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
            ("repo-templates/web-app/Dockerfile".to_string(), "FROM x\n".to_string()),
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
}
