//! Ephemeral environments (§8, §13) — PR-scoped previews on the private node.
//!
//! Flow: PR opened → GHA builds & pushes `pr-<N>` → `registry_package` event
//! lands here → manifest generated (base ⊕ ephemeral overlay ⊕ PR patch:
//! digest, name `<app>-pr<N>`, host `<app>-pr<N>.<project>.majksa.net`) →
//! committed onto `env/ephemeral` → reconciler deploys → preview URL
//! commented on the PR. PR closed → manifest removed → reconciler GC
//! (48 h grace, 7 d hard TTL).
//!
//! An app opts into previews by having `apps/<app>/ephemeral.yaml`; manifests
//! are always generated, never hand-written (§8). Commits to `env/ephemeral`
//! are direct (ADR 0003) — the branch stays the uniform deploy trigger.

use anyhow::{Context, Result};
use majnet_common::{manifest::AppManifest, merge::merge, EnvClass};
use std::collections::BTreeMap;

use crate::AppState;

const COMMENT_MARKER: &str = "<!-- majnet-preview -->";

/// A `pr-<N>` image landed in GHCR: render + deploy the preview.
pub async fn on_pr_build(
    state: &AppState,
    org: &str,
    app: &str,
    pr: u64,
    image: &str,
) -> Result<()> {
    let (_, tarball) = crate::proxy::fetch_snapshot(state, org, "ops", "main").await?;
    let sources = majnet_common::tarball::untar(&tarball)?;

    let Some(overlay) = sources.get(&format!("apps/{app}/ephemeral.yaml")) else {
        tracing::info!(
            org,
            app,
            pr,
            "no ephemeral overlay — previews not enabled for this app"
        );
        return Ok(());
    };
    let base = sources
        .get(&format!("apps/{app}/base.yaml"))
        .with_context(|| format!("apps/{app}/base.yaml missing"))?;

    let project = project_name(state, org).await?;
    let (yaml, preview_url) = generate_manifest(base, overlay, &project, app, pr, image)?;
    let name = format!("{app}-pr{pr}");

    // Secrets: previews reuse the app's stable-class secrets (same key class,
    // same recipients) — referenced, never duplicated.
    let mut changes = BTreeMap::from([(format!("{name}.yaml"), Some(yaml))]);
    if let Some(secrets) = sources.get(&format!("apps/{app}/secrets.stable.yaml")) {
        changes.insert(
            format!("secrets/{name}.yaml"),
            Some(String::from_utf8(secrets.clone()).context("secrets not UTF-8")?),
        );
    }

    commit_to_ephemeral(
        state,
        org,
        changes,
        &format!("preview({app}): pr-{pr} @ {}", short(image)),
    )
    .await?;
    state
        .store
        .log_event("ephemeral-deploy", Some(org), &format!("{name} → {image}"))?;

    if let Some(url) = preview_url {
        comment_preview_url(state, org, app, pr, &url).await?;
    }
    Ok(())
}

/// PR closed/merged: remove the manifest (reconciler grace-GCs the stack).
pub async fn on_pr_closed(state: &AppState, org: &str, app: &str, pr: u64) -> Result<()> {
    let name = format!("{app}-pr{pr}");
    let changes = BTreeMap::from([
        (format!("{name}.yaml"), None),
        (format!("secrets/{name}.yaml"), None),
    ]);
    match commit_to_ephemeral(
        state,
        org,
        changes,
        &format!("preview({app}): remove pr-{pr}"),
    )
    .await
    {
        Ok(()) => {
            state
                .store
                .log_event("ephemeral-remove", Some(org), &name)?;
            Ok(())
        }
        // Nothing was ever deployed for this PR (e.g. previews disabled).
        Err(e) if format!("{e:#}").contains("nothing to change") => Ok(()),
        Err(e) => Err(e),
    }
}

/// base ⊕ ephemeral overlay ⊕ PR patch → validated manifest YAML + preview URL.
fn generate_manifest(
    base: &[u8],
    overlay: &[u8],
    project: &str,
    app: &str,
    pr: u64,
    image: &str,
) -> Result<(String, Option<String>)> {
    let base: serde_yaml::Value = serde_yaml::from_slice(base).context("base.yaml")?;
    let overlay: serde_yaml::Value = serde_yaml::from_slice(overlay).context("ephemeral.yaml")?;
    let mut merged = merge(base, overlay);

    let name = format!("{app}-pr{pr}");
    let host = format!("{name}.{project}.majksa.net");
    let map = merged
        .as_mapping_mut()
        .context("merged manifest is not a mapping")?;
    map.insert("name".into(), name.clone().into());
    map.insert("image".into(), image.into());

    // Previews get their own host; apps without ingress deploy silently.
    let mut preview_url = None;
    if let Some(ingress) = map
        .get_mut(serde_yaml::Value::from("ingress"))
        .and_then(|i| i.as_mapping_mut())
    {
        ingress.insert("host".into(), host.clone().into());
        preview_url = Some(format!("https://{host}"));
    }

    let yaml = serde_yaml::to_string(&merged)?;
    AppManifest::parse(&yaml).context("generated ephemeral manifest invalid")?;
    Ok((yaml, preview_url))
}

/// Direct commit onto env/ephemeral (ADR 0003). Creates the branch if absent.
async fn commit_to_ephemeral(
    state: &AppState,
    org: &str,
    changes: BTreeMap<String, Option<String>>,
    message: &str,
) -> Result<()> {
    let client = state.github.org_client(org).await?;
    let repo = format!("/repos/{org}/ops");
    let branch = EnvClass::Ephemeral.env_branch();

    match crate::git::get_branch_head(&client, &repo, &branch).await? {
        Some(head) => {
            let base_tree = crate::git::commit_tree(&client, &repo, &head).await?;
            let has_additions = changes.values().any(Option::is_some);
            let tree = if has_additions {
                crate::git::create_tree_incremental(&client, &repo, &base_tree, &changes).await?
            } else {
                // Deletion-only trees fail on paths that don't exist — filter
                // against the current tree first.
                let current: serde_json::Value = client
                    .get(
                        format!("{repo}/git/trees/{base_tree}?recursive=1"),
                        None::<&()>,
                    )
                    .await?;
                let existing: Vec<&str> = current["tree"]
                    .as_array()
                    .map(|t| t.iter().filter_map(|e| e["path"].as_str()).collect())
                    .unwrap_or_default();
                let deletions: BTreeMap<String, Option<String>> = changes
                    .into_iter()
                    .filter(|(path, _)| existing.contains(&path.as_str()))
                    .collect();
                anyhow::ensure!(!deletions.is_empty(), "nothing to change on env/ephemeral");
                crate::git::create_tree_incremental(&client, &repo, &base_tree, &deletions).await?
            };
            if tree == base_tree {
                return Ok(());
            }
            let commit =
                crate::git::create_commit(&client, &repo, &tree, &[&head], message).await?;
            crate::git::force_update_ref(&client, &repo, &branch, &commit).await?;
        }
        None => {
            let additions: BTreeMap<String, String> = changes
                .into_iter()
                .filter_map(|(p, c)| Some((p, c?)))
                .collect();
            anyhow::ensure!(!additions.is_empty(), "nothing to change on env/ephemeral");
            let tree = crate::git::create_tree(&client, &repo, &additions).await?;
            let commit = crate::git::create_commit(&client, &repo, &tree, &[], message).await?;
            crate::git::create_ref(&client, &repo, &branch, &commit).await?;
        }
    }
    Ok(())
}

/// Preview URL as a PR comment — once per PR, updated in place on new digests.
async fn comment_preview_url(
    state: &AppState,
    org: &str,
    app: &str,
    pr: u64,
    url: &str,
) -> Result<()> {
    let client = state.github.org_client(org).await?;
    let body = format!(
        "{COMMENT_MARKER}\n🚀 Preview deployed: {url}\n\n_Updates on every push; removed 48 h after this PR closes (7 d hard limit)._"
    );

    let comments: Vec<serde_json::Value> = client
        .get(
            format!("/repos/{org}/{app}/issues/{pr}/comments?per_page=100"),
            None::<&()>,
        )
        .await
        .unwrap_or_default();
    if let Some(existing) = comments
        .iter()
        .find(|c| c["body"].as_str().unwrap_or("").starts_with(COMMENT_MARKER))
    {
        let id = existing["id"].as_u64().context("comment has no id")?;
        let _: serde_json::Value = client
            .patch(
                format!("/repos/{org}/{app}/issues/comments/{id}"),
                Some(&serde_json::json!({ "body": body })),
            )
            .await?;
    } else {
        let _: serde_json::Value = client
            .post(
                format!("/repos/{org}/{app}/issues/{pr}/comments"),
                Some(&serde_json::json!({ "body": body })),
            )
            .await?;
    }
    Ok(())
}

/// Project name for an org, from the root registry.
async fn project_name(state: &AppState, org: &str) -> Result<String> {
    let (_, tarball) =
        crate::proxy::fetch_snapshot(state, &state.config.root_org, "platform", "main").await?;
    let platform = majnet_common::tarball::untar(&tarball)?;
    let projects = majnet_common::platform::ProjectsFile::parse(
        platform
            .get("projects.yaml")
            .context("platform repo has no projects.yaml")?,
    )?;
    projects
        .projects
        .iter()
        .find(|p| p.org == org)
        .map(|p| p.name.clone())
        .with_context(|| format!("org '{org}' not in registry"))
}

fn short(image: &str) -> &str {
    &image[image.len().saturating_sub(12)..]
}

#[cfg(test)]
mod tests {
    use super::generate_manifest;

    const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn patches_name_image_and_host() {
        let base =
            b"env:\n  RUST_LOG: info\ningress:\n  host: api.zpevnik.majksa.net\n  port: 8080\n";
        let overlay = b"env:\n  MODE: preview\n";
        let image = format!("ghcr.io/zpevnik/api@{DIGEST}");
        let (yaml, url) = generate_manifest(base, overlay, "zpevnik", "api", 12, &image).unwrap();
        assert!(yaml.contains("name: api-pr12"));
        assert!(yaml.contains("host: api-pr12.zpevnik.majksa.net"));
        assert!(yaml.contains("MODE: preview"));
        assert_eq!(url.as_deref(), Some("https://api-pr12.zpevnik.majksa.net"));
    }

    #[test]
    fn no_ingress_means_no_preview_url() {
        let image = format!("ghcr.io/o/worker@{DIGEST}");
        let (yaml, url) =
            generate_manifest(b"env: {}\n", b"{}\n", "p", "worker", 3, &image).unwrap();
        assert!(yaml.contains("name: worker-pr3"));
        assert!(url.is_none());
    }

    #[test]
    fn tag_pinned_patch_is_rejected() {
        assert!(generate_manifest(b"{}\n", b"{}\n", "p", "a", 1, "ghcr.io/o/a:pr-1").is_err());
    }
}
