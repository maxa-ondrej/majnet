//! Manifest rendering (§9, §11.5).
//!
//! On every ops `main` push: merge `base.yaml` ⊕ class overlay per app,
//! validate strictly, and commit the **complete rendered tree** to a
//! `render/<class>` branch, opening (or fast-forwarding) a render PR against
//! `env/<class>`. Secrets pass through encrypted — rendering never decrypts.
//!
//! Merge policy: `stable` render PRs auto-merge (preserving auto-deploy);
//! `env/production` waits for admin review — that review IS the production
//! gate, over the most truthful artifact possible: the exact final diff.
//! Multiple `main` pushes while a PR is open fast-forward the same PR.
//!
//! An overlay file's presence (`apps/<app>/<class>.yaml`) opts the app into
//! that class. Ephemeral manifests are generated from stable + PR patch in
//! phase 4, never rendered from main.
//!
//! Any validation failure aborts the WHOLE class render loudly: the env
//! branches are full-tree replaces, so skipping one bad app would silently
//! undeploy it.

use anyhow::{bail, ensure, Context, Result};
use majnet_common::{merge::merge, manifest::AppManifest, EnvClass};
use serde_json::json;
use std::collections::BTreeMap;

use crate::AppState;

/// Classes rendered from ops `main` in phase 2.
const RENDERED_CLASSES: [EnvClass; 2] = [EnvClass::Stable, EnvClass::Production];

pub async fn on_ops_main_push(state: &AppState, org: &str, commit: &str) -> Result<()> {
    let (_, tarball) = crate::proxy::fetch_snapshot(state, org, "ops", "main").await?;
    let sources = majnet_common::tarball::untar(&tarball)?;

    for class in RENDERED_CLASSES {
        match render_class(&sources, class).with_context(|| format!("rendering {}", class.as_str()))? {
            Some(rendered) => {
                push_render_pr(state, org, class, commit, rendered).await?;
            }
            None => tracing::debug!(org, class = class.as_str(), "no apps opt into this class"),
        }
    }
    Ok(())
}

/// Pure render: sources tree → rendered env-branch tree. None = class empty.
fn render_class(sources: &BTreeMap<String, Vec<u8>>, class: EnvClass) -> Result<Option<BTreeMap<String, String>>> {
    let mut rendered = BTreeMap::new();

    let apps: Vec<&str> = sources
        .keys()
        .filter_map(|path| path.strip_prefix("apps/")?.strip_suffix("/base.yaml"))
        .filter(|app| !app.contains('/'))
        .collect();

    for app in apps {
        let overlay_path = format!("apps/{app}/{}.yaml", class.as_str());
        let Some(overlay_bytes) = sources.get(&overlay_path) else {
            continue; // overlay presence opts the app into the class
        };
        let base_bytes = &sources[&format!("apps/{app}/base.yaml")];

        let base: serde_yaml::Value = serde_yaml::from_slice(base_bytes).with_context(|| format!("{app}: base.yaml"))?;
        let overlay: serde_yaml::Value = serde_yaml::from_slice(overlay_bytes).with_context(|| format!("{app}: {overlay_path}"))?;
        let mut merged = merge(base, overlay);

        // The app's identity is its directory; a conflicting `name` is a bug.
        if let serde_yaml::Value::Mapping(map) = &mut merged {
            let name_key = serde_yaml::Value::from("name");
            match map.get(&name_key).and_then(|v| v.as_str()) {
                None => {
                    map.insert(name_key, serde_yaml::Value::from(app));
                }
                Some(existing) => ensure!(existing == app, "{app}: manifest name '{existing}' does not match app directory"),
            }
        } else {
            bail!("{app}: merged manifest is not a mapping");
        }

        let yaml = serde_yaml::to_string(&merged)?;
        let manifest = AppManifest::parse(&yaml).with_context(|| format!("{app}: rendered manifest invalid"))?;

        // Secrets pass through encrypted, and every declared secret must exist.
        let secrets_path = format!("apps/{app}/secrets.{}.yaml", class.as_str());
        match sources.get(&secrets_path) {
            Some(bytes) => {
                rendered.insert(format!("secrets/{app}.yaml"), String::from_utf8(bytes.clone()).context("secrets file is not UTF-8")?);
            }
            None => ensure!(manifest.secrets.is_empty(), "{app}: declares secrets but {secrets_path} is missing"),
        }
        rendered.insert(format!("{app}.yaml"), yaml);
    }

    Ok((!rendered.is_empty()).then_some(rendered))
}

/// Commit the rendered tree to `render/<class>` and ensure a render PR onto
/// `env/<class>` exists (auto-merged for stable).
async fn push_render_pr(
    state: &AppState,
    org: &str,
    class: EnvClass,
    source_commit: &str,
    files: BTreeMap<String, String>,
) -> Result<()> {
    let client = state.github.org_client(org).await?;
    let env_branch = class.env_branch();
    let render_branch = format!("render/{}", class.as_str());
    let repo = format!("/repos/{org}/ops");

    // Full replacement tree — env branches contain exactly the render output.
    let tree_sha = &crate::git::create_tree(&client, &repo, &files).await.context("creating rendered tree")?;

    // Ensure the env branch exists (orphan history: first render is the root).
    let env_head = crate::git::get_branch_head(&client, &repo, &env_branch).await?;
    let env_head = match env_head {
        Some(sha) => sha,
        None => {
            let commit = crate::git::create_commit(&client, &repo, tree_sha, &[], &format!("render: initial {} tree", class.as_str())).await?;
            crate::git::create_ref(&client, &repo, &env_branch, &commit).await?;
            tracing::info!(org, branch = env_branch, "created env branch with initial render");
            return Ok(());
        }
    };

    // No-op renders create no PR noise.
    let env_tree: serde_json::Value = client.get(format!("{repo}/git/commits/{env_head}"), None::<&()>).await?;
    if env_tree["tree"]["sha"].as_str() == Some(tree_sha) {
        tracing::info!(org, class = class.as_str(), "render identical to env branch, nothing to do");
        return Ok(());
    }

    let message = format!("render({}): from main@{}", class.as_str(), &source_commit[..12.min(source_commit.len())]);
    let commit_sha = crate::git::create_commit(&client, &repo, tree_sha, &[&env_head], &message).await?;

    // Point render/<class> at the new commit (force: pending changes accumulate
    // into the same PR, always as a single rendered state).
    if crate::git::get_branch_head(&client, &repo, &render_branch).await?.is_some() {
        crate::git::force_update_ref(&client, &repo, &render_branch, &commit_sha).await?;
    } else {
        crate::git::create_ref(&client, &repo, &render_branch, &commit_sha).await?;
    }

    // One open render PR per class.
    let open: serde_json::Value = client
        .get(
            format!("{repo}/pulls?state=open&base={env_branch}&head={org}:{render_branch}"),
            None::<&()>,
        )
        .await?;
    let pr_number = match open.as_array().and_then(|prs| prs.first()) {
        Some(pr) => pr["number"].as_u64().context("PR has no number")?,
        None => {
            let pr: serde_json::Value = client
                .post(
                    format!("{repo}/pulls"),
                    Some(&json!({
                        "title": format!("render: {}", class.as_str()),
                        "head": render_branch,
                        "base": env_branch,
                        "body": format!(
                            "Rendered manifests from `main@{source_commit}`.\n\n\
                             Merging this PR **is the deploy trigger** for `{}`.",
                            class.as_str()
                        ),
                    })),
                )
                .await
                .context("opening render PR")?;
            pr["number"].as_u64().context("PR has no number")?
        }
    };

    state.store.log_event("render-pr", Some(org), &format!("{} PR #{pr_number} @ {commit_sha}", class.as_str()))?;

    if class.auto_merges() {
        let _: serde_json::Value = client
            .put(
                format!("{repo}/pulls/{pr_number}/merge"),
                Some(&json!({ "merge_method": "merge", "sha": commit_sha })),
            )
            .await
            .with_context(|| format!("auto-merging render PR #{pr_number}"))?;
        tracing::info!(org, class = class.as_str(), pr_number, "render PR auto-merged (deploy trigger)");
    } else {
        tracing::info!(org, class = class.as_str(), pr_number, "render PR awaits admin review (production gate)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn sources(entries: &[(&str, &str)]) -> BTreeMap<String, Vec<u8>> {
        entries.iter().map(|(k, v)| (k.to_string(), v.as_bytes().to_vec())).collect()
    }

    #[test]
    fn renders_only_opted_in_apps() {
        let src = sources(&[
            ("apps/api/base.yaml", "env:\n  RUST_LOG: info\n"),
            ("apps/api/stable.yaml", &format!("image: ghcr.io/o/api@{DIGEST}\n")),
            ("apps/web/base.yaml", "env: {}\n"), // no stable overlay → not rendered
        ]);
        let rendered = render_class(&src, EnvClass::Stable).unwrap().unwrap();
        assert_eq!(rendered.keys().collect::<Vec<_>>(), vec!["api.yaml"]);
        assert!(rendered["api.yaml"].contains("name: api"));
        assert!(rendered["api.yaml"].contains("RUST_LOG"));
    }

    #[test]
    fn empty_class_renders_none() {
        let src = sources(&[("apps/api/base.yaml", "env: {}\n")]);
        assert!(render_class(&src, EnvClass::Production).unwrap().is_none());
    }

    #[test]
    fn invalid_manifest_fails_whole_class() {
        let src = sources(&[
            ("apps/api/base.yaml", ""),
            ("apps/api/stable.yaml", "image: ghcr.io/o/api:latest\n"), // tag, not digest
        ]);
        assert!(render_class(&src, EnvClass::Stable).is_err());
    }

    #[test]
    fn declared_secrets_require_secrets_file() {
        let src = sources(&[
            ("apps/api/base.yaml", "secrets: [db-url]\n"),
            ("apps/api/stable.yaml", &format!("image: ghcr.io/o/api@{DIGEST}\n")),
        ]);
        assert!(render_class(&src, EnvClass::Stable).unwrap_err().to_string().contains("secrets"));
    }

    #[test]
    fn secrets_pass_through_encrypted() {
        let src = sources(&[
            ("apps/api/base.yaml", "secrets: [db-url]\n"),
            ("apps/api/stable.yaml", &format!("image: ghcr.io/o/api@{DIGEST}\n")),
            ("apps/api/secrets.stable.yaml", "db-url: ENC[AES256_GCM,...]\n"),
        ]);
        let rendered = render_class(&src, EnvClass::Stable).unwrap().unwrap();
        assert_eq!(rendered["secrets/api.yaml"], "db-url: ENC[AES256_GCM,...]\n");
    }

    #[test]
    fn name_mismatch_is_rejected() {
        let src = sources(&[
            ("apps/api/base.yaml", "name: other\n"),
            ("apps/api/stable.yaml", &format!("image: ghcr.io/o/api@{DIGEST}\n")),
        ]);
        assert!(render_class(&src, EnvClass::Stable).unwrap_err().to_string().contains("does not match"));
    }
}
