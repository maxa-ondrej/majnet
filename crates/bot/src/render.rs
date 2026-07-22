//! Manifest rendering (§9, §11.5).
//!
//! On every ops `main` push: merge `base.yaml` ⊕ class overlay per app,
//! validate strictly, and commit the **complete rendered tree** to a
//! `render/<class>` branch, opening (or fast-forwarding) a render PR against
//! `env/<class>`. Secrets pass through encrypted — rendering never decrypts.
//!
//! Merge policy: `testing`/`stable` render PRs auto-merge (preserving
//! auto-deploy); `env/production` waits for admin review — that review IS the
//! production gate, over the most truthful artifact possible: the exact final diff.
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
use base64::Engine;
use majnet_common::{manifest::AppManifest, merge::merge, EnvClass};
use serde_json::json;
use std::collections::BTreeMap;

use crate::AppState;

/// The persistent classes rendered from ops `main` (ephemeral renders per-PR,
/// separately). `testing` + `stable` auto-merge; `production` gates (§9).
const RENDERED_CLASSES: [EnvClass; 3] = [EnvClass::Testing, EnvClass::Stable, EnvClass::Production];

pub async fn on_ops_main_push(state: &AppState, org: &str, commit: &str) -> Result<()> {
    let (_, tarball) = crate::proxy::fetch_snapshot(state, org, "ops", "main").await?;
    let sources = majnet_common::tarball::untar(&tarball)?;
    // ADR 0013: non-production classes get an auto-assigned ingress host built
    // from the project name + platform base domain.
    let (project, base_domain) = crate::dashboard_api::project_and_domain(state, org).await?;

    for class in RENDERED_CLASSES {
        match render_class(&sources, class, &project, &base_domain)
            .with_context(|| format!("rendering {}", class.as_str()))?
        {
            Some(rendered) => {
                // Production domains get their Cloudflare edge wiring ensured
                // before the render PR (ADR 0007). Non-fatal.
                if class == EnvClass::Production {
                    if let Err(e) = crate::cloudflare::ensure_domains(state, &rendered).await {
                        tracing::error!(org, error = format!("{e:#}"), "Cloudflare ensure failed");
                    }
                }
                push_render_pr(state, org, class, commit, rendered).await?;
            }
            None => tracing::debug!(org, class = class.as_str(), "no apps opt into this class"),
        }
    }
    Ok(())
}

/// The VPN-class (tailnet) ingress hosts a project serves from its private-node
/// Traefik — every host across the persistent VPN classes (`testing`, `stable`).
/// Empty ⇒ the project has no tailnet ingress, so it needs no split-DNS CNAME
/// and no ingress cert (used by org-sync to gate + skip production-only projects).
///
/// Renders the ops `main` tree exactly as a real render would (same auto-host
/// assignment), so the hosts match what the reconciler deploys. Ephemeral
/// PR-preview hosts are dynamic (not in `main`) and intentionally excluded — they
/// are covered by the wildcard split-DNS CNAME + wildcard cert, not enumerated.
pub async fn project_ingress_hosts(
    state: &AppState,
    org: &str,
    project: &str,
    base_domain: &str,
) -> Result<Vec<String>> {
    let (_, tarball) = crate::proxy::fetch_snapshot(state, org, "ops", "main").await?;
    let sources = majnet_common::tarball::untar(&tarball)?;
    vpn_ingress_hosts(&sources, project, base_domain)
}

/// Pure core of [`project_ingress_hosts`]: collect the tailnet ingress hosts
/// across the persistent VPN classes from an ops `main` sources tree.
fn vpn_ingress_hosts(
    sources: &BTreeMap<String, Vec<u8>>,
    project: &str,
    base_domain: &str,
) -> Result<Vec<String>> {
    const VPN_CLASSES: [EnvClass; 2] = [EnvClass::Testing, EnvClass::Stable];
    let mut hosts = std::collections::BTreeSet::new();
    for class in VPN_CLASSES {
        let Some(rendered) = render_class(sources, class, project, base_domain)? else {
            continue;
        };
        for (path, yaml) in &rendered {
            if path.starts_with("secrets/") {
                continue; // rendered secrets file, not a manifest
            }
            let manifest = AppManifest::parse(yaml)
                .with_context(|| format!("{path}: rendered manifest (ingress-host scan)"))?;
            if let Some(ingress) = &manifest.ingress {
                hosts.extend(ingress.hosts().into_iter().map(str::to_string));
            }
        }
    }
    Ok(hosts.into_iter().collect())
}

/// Pure render: sources tree → rendered env-branch tree. None = class empty.
fn render_class(
    sources: &BTreeMap<String, Vec<u8>>,
    class: EnvClass,
    project: &str,
    base_domain: &str,
) -> Result<Option<BTreeMap<String, String>>> {
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

        let base: serde_yaml::Value =
            serde_yaml::from_slice(base_bytes).with_context(|| format!("{app}: base.yaml"))?;
        let overlay: serde_yaml::Value = serde_yaml::from_slice(overlay_bytes)
            .with_context(|| format!("{app}: {overlay_path}"))?;
        let mut merged = merge(base, overlay);

        // The app's identity is its directory; a conflicting `name` is a bug.
        if let serde_yaml::Value::Mapping(map) = &mut merged {
            let name_key = serde_yaml::Value::from("name");
            match map.get(&name_key).and_then(|v| v.as_str()) {
                None => {
                    map.insert(name_key, serde_yaml::Value::from(app));
                }
                Some(existing) => ensure!(
                    existing == app,
                    "{app}: manifest name '{existing}' does not match app directory"
                ),
            }
            // ADR 0013: non-production classes get an auto-assigned ingress host
            // (`{app}.{project}.{base_domain}`); the app declares only the port,
            // and any custom host/domains it carried are ignored here. Production
            // keeps its custom host/domains (Cloudflare + edge, ADR 0007).
            if class != EnvClass::Production {
                if let Some(ingress) = map
                    .get_mut(serde_yaml::Value::from("ingress"))
                    .and_then(|i| i.as_mapping_mut())
                {
                    let host = format!("{app}.{project}.{base_domain}");
                    ingress.insert("host".into(), host.into());
                    ingress.remove(serde_yaml::Value::from("domains"));
                }
            }
        } else {
            bail!("{app}: merged manifest is not a mapping");
        }

        let yaml = serde_yaml::to_string(&merged)?;
        // Validate the rendered manifest; secrets travel inside it as inline
        // `majnet:` envelopes (ADR 0024) — rendering never decrypts, no sidecar file.
        let manifest = AppManifest::parse(&yaml)
            .with_context(|| format!("{app}: rendered manifest invalid"))?;
        // SOPS was retired (ADR 0024 phase 3): a legacy bare-name `secrets:` list has
        // no delivery path, so reject it at render (converge would fail too).
        ensure!(
            manifest.secrets.names().is_none(),
            "{app}: legacy `secrets:` name list is no longer supported (ADR 0024) — use an inline `secrets:` map"
        );
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
    let tree_sha = &crate::git::create_tree(&client, &repo, &files)
        .await
        .context("creating rendered tree")?;

    // Ensure the env branch exists (orphan history: first render is the root).
    let env_head = crate::git::get_branch_head(&client, &repo, &env_branch).await?;
    let env_head = match env_head {
        Some(sha) => sha,
        None => {
            let commit = crate::git::create_commit(
                &client,
                &repo,
                tree_sha,
                &[],
                &format!("render: initial {} tree", class.as_str()),
            )
            .await?;
            crate::git::create_ref(&client, &repo, &env_branch, &commit).await?;
            tracing::info!(
                org,
                branch = env_branch,
                "created env branch with initial render"
            );
            return Ok(());
        }
    };

    // No-op renders create no PR noise.
    let env_tree: serde_json::Value = client
        .get(format!("{repo}/git/commits/{env_head}"), None::<&()>)
        .await?;
    if env_tree["tree"]["sha"].as_str() == Some(tree_sha) {
        tracing::info!(
            org,
            class = class.as_str(),
            "render identical to env branch, nothing to do"
        );
        return Ok(());
    }

    let message = format!(
        "render({}): from main@{}",
        class.as_str(),
        &source_commit[..12.min(source_commit.len())]
    );
    let commit_sha =
        crate::git::create_commit(&client, &repo, tree_sha, &[&env_head], &message).await?;

    // Point render/<class> at the new commit (force: pending changes accumulate
    // into the same PR, always as a single rendered state).
    if crate::git::get_branch_head(&client, &repo, &render_branch)
        .await?
        .is_some()
    {
        crate::git::force_update_ref(&client, &repo, &render_branch, &commit_sha).await?;
    } else {
        crate::git::create_ref(&client, &repo, &render_branch, &commit_sha).await?;
    }

    // Human-readable version diff (falls back to short digests) for the PR body.
    let summary = version_summary(state, &client, org, &env_branch, &files).await;
    let body = format!(
        "Rendered manifests from `main@{source_commit}`.\n\n\
         Merging this PR **is the deploy trigger** for `{}`.{}",
        class.as_str(),
        summary.unwrap_or_default()
    );

    // One open render PR per class.
    let open: serde_json::Value = client
        .get(
            format!("{repo}/pulls?state=open&base={env_branch}&head={org}:{render_branch}"),
            None::<&()>,
        )
        .await?;
    let pr_number = match open.as_array().and_then(|prs| prs.first()) {
        Some(pr) => {
            let n = pr["number"].as_u64().context("PR has no number")?;
            // Keep the body current as pending changes accumulate into this PR.
            let _: std::result::Result<serde_json::Value, _> = client
                .patch(format!("{repo}/pulls/{n}"), Some(&json!({ "body": body })))
                .await;
            n
        }
        None => {
            let pr: serde_json::Value = client
                .post(
                    format!("{repo}/pulls"),
                    Some(&json!({
                        "title": format!("render: {}", class.as_str()),
                        "head": render_branch,
                        "base": env_branch,
                        "body": body,
                    })),
                )
                .await
                .context("opening render PR")?;
            pr["number"].as_u64().context("PR has no number")?
        }
    };

    state.store.log_event(
        "render-pr",
        Some(org),
        &format!("{} PR #{pr_number} @ {commit_sha}", class.as_str()),
    )?;

    if class.auto_merges() {
        let _: serde_json::Value = client
            .put(
                format!("{repo}/pulls/{pr_number}/merge"),
                Some(&json!({ "merge_method": "merge", "sha": commit_sha })),
            )
            .await
            .with_context(|| format!("auto-merging render PR #{pr_number}"))?;
        tracing::info!(
            org,
            class = class.as_str(),
            pr_number,
            "render PR auto-merged (deploy trigger)"
        );
    } else {
        tracing::info!(
            org,
            class = class.as_str(),
            pr_number,
            "render PR awaits admin review (production gate)"
        );
    }
    Ok(())
}

/// The `image:` of a rendered manifest, if present.
fn image_of(yaml: &str) -> Option<String> {
    serde_yaml::from_str::<serde_yaml::Value>(yaml)
        .ok()?
        .get("image")?
        .as_str()
        .map(str::to_string)
}

/// A short human ref for an image: its recorded release version if known, else
/// the first 12 hex of the digest.
fn short_ref(store: &crate::state::Store, org: &str, app: &str, image: &str) -> String {
    if let Ok(Some(v)) = store.version_for_image(org, app, image) {
        return v;
    }
    match image.split_once("@sha256:") {
        Some((_, digest)) => digest.chars().take(12).collect(),
        None => image.to_string(),
    }
}

/// A markdown "Version changes" section for the render PR body: one row per app
/// whose rendered image differs from the current env branch, showing the release
/// version (or short digest) old → new. Best-effort — any read/parse hiccup just
/// omits that app; returns None when nothing changed.
async fn version_summary(
    state: &AppState,
    client: &octocrab::Octocrab,
    org: &str,
    env_branch: &str,
    files: &BTreeMap<String, String>,
) -> Option<String> {
    let repos = client.repos(org, "ops");
    let mut rows = Vec::new();
    for (path, new_content) in files {
        // Only top-level `<app>.yaml` manifests (skip `secrets/…`).
        let Some(app) = path.strip_suffix(".yaml").filter(|p| !p.contains('/')) else {
            continue;
        };
        let Some(new_image) = image_of(new_content) else {
            continue;
        };
        // Current image on the env branch (None = newly added app).
        let old_image = match repos
            .get_content()
            .path(path)
            .r#ref(env_branch)
            .send()
            .await
        {
            Ok(content) => content
                .items
                .into_iter()
                .next()
                .and_then(|item| item.content)
                .map(|c| c.replace(['\n', ' '], ""))
                .and_then(|c| base64::engine::general_purpose::STANDARD.decode(c).ok())
                .and_then(|b| String::from_utf8(b).ok())
                .and_then(|y| image_of(&y)),
            Err(_) => None,
        };
        if old_image.as_deref() == Some(new_image.as_str()) {
            continue; // unchanged
        }
        let new_ref = short_ref(&state.store, org, app, &new_image);
        let old_ref = match &old_image {
            Some(img) => short_ref(&state.store, org, app, img),
            None => "—".to_string(),
        };
        rows.push(format!("| `{app}` | `{old_ref}` → `{new_ref}` |"));
    }
    if rows.is_empty() {
        return None;
    }
    rows.sort();
    Some(format!(
        "\n\n**Version changes**\n\n| app | change |\n|---|---|\n{}",
        rows.join("\n")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn sources(entries: &[(&str, &str)]) -> BTreeMap<String, Vec<u8>> {
        entries
            .iter()
            .map(|(k, v)| (k.to_string(), v.as_bytes().to_vec()))
            .collect()
    }

    #[test]
    fn renders_only_opted_in_apps() {
        let src = sources(&[
            ("apps/api/base.yaml", "env:\n  RUST_LOG: info\n"),
            (
                "apps/api/stable.yaml",
                &format!("image: ghcr.io/o/api@{DIGEST}\n"),
            ),
            ("apps/web/base.yaml", "env: {}\n"), // no stable overlay → not rendered
        ]);
        let rendered = render_class(&src, EnvClass::Stable, "proj", "majksa.net")
            .unwrap()
            .unwrap();
        assert_eq!(rendered.keys().collect::<Vec<_>>(), vec!["api.yaml"]);
        assert!(rendered["api.yaml"].contains("name: api"));
        assert!(rendered["api.yaml"].contains("RUST_LOG"));
    }

    #[test]
    fn empty_class_renders_none() {
        let src = sources(&[("apps/api/base.yaml", "env: {}\n")]);
        assert!(
            render_class(&src, EnvClass::Production, "proj", "majksa.net")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn invalid_manifest_fails_whole_class() {
        let src = sources(&[
            ("apps/api/base.yaml", ""),
            ("apps/api/stable.yaml", "image: ghcr.io/o/api:latest\n"), // tag, not digest
        ]);
        assert!(render_class(&src, EnvClass::Stable, "proj", "majksa.net").is_err());
    }

    #[test]
    fn rejects_legacy_name_list_secrets() {
        // SOPS retired (ADR 0024): a bare-name `secrets:` list has no delivery path.
        let src = sources(&[
            ("apps/api/base.yaml", "secrets: [db-url]\n"),
            (
                "apps/api/stable.yaml",
                &format!("image: ghcr.io/o/api@{DIGEST}\n"),
            ),
        ]);
        assert!(render_class(&src, EnvClass::Stable, "proj", "majksa.net")
            .unwrap_err()
            .to_string()
            .contains("inline"));
    }

    #[test]
    fn inline_secrets_travel_in_the_manifest() {
        let src = sources(&[
            ("apps/api/base.yaml", "secrets:\n  DB_URL: majnet:AgABC=\n"),
            (
                "apps/api/stable.yaml",
                &format!("image: ghcr.io/o/api@{DIGEST}\n"),
            ),
        ]);
        let rendered = render_class(&src, EnvClass::Stable, "proj", "majksa.net")
            .unwrap()
            .unwrap();
        // Inline map is in the rendered manifest; no sidecar secrets file.
        assert!(rendered["api.yaml"].contains("DB_URL: majnet:AgABC="));
        assert!(!rendered.contains_key("secrets/api.yaml"));
    }

    #[test]
    fn vpn_ingress_hosts_collects_auto_hosts_and_ignores_prod_and_hostless() {
        let src = sources(&[
            // A stable service with an ingress → gets an auto-assigned VPN host.
            ("apps/grafana/base.yaml", "ingress:\n  port: 3000\n"),
            (
                "apps/grafana/stable.yaml",
                &format!("image: ghcr.io/o/grafana@{DIGEST}\n"),
            ),
            // A production-only app → NOT a VPN host (production is edge, not tailnet).
            (
                "apps/web/base.yaml",
                "ingress:\n  host: web.example.com\n  port: 80\n",
            ),
            (
                "apps/web/production.yaml",
                &format!("image: ghcr.io/o/web@{DIGEST}\n"),
            ),
            // A stable app with no ingress → contributes no host.
            ("apps/worker/base.yaml", "resources:\n  memory: 64m\n"),
            (
                "apps/worker/stable.yaml",
                &format!("image: ghcr.io/o/worker@{DIGEST}\n"),
            ),
        ]);
        let hosts = super::vpn_ingress_hosts(&src, "majnet", "majksa.net").unwrap();
        assert_eq!(hosts, vec!["grafana.majnet.majksa.net".to_string()]);
    }

    #[test]
    fn vpn_ingress_hosts_empty_for_production_only_project() {
        // A project whose apps render only for production has no tailnet ingress.
        let src = sources(&[
            (
                "apps/api/base.yaml",
                "ingress:\n  host: api.example.com\n  port: 8080\n",
            ),
            (
                "apps/api/production.yaml",
                &format!("image: ghcr.io/o/api@{DIGEST}\n"),
            ),
        ]);
        assert!(super::vpn_ingress_hosts(&src, "sideline", "majksa.net")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn non_production_gets_an_auto_assigned_host() {
        // The app declares only a port; base carries a stray host + domains that
        // must be ignored for non-production (ADR 0013).
        let src = sources(&[
            (
                "apps/api/base.yaml",
                "ingress:\n  host: leftover.example.com\n  port: 8080\n  domains:\n    - www.example.com\n",
            ),
            (
                "apps/api/stable.yaml",
                &format!("image: ghcr.io/o/api@{DIGEST}\n"),
            ),
        ]);
        let rendered = render_class(&src, EnvClass::Stable, "zpevnik", "majksa.net")
            .unwrap()
            .unwrap();
        let m = AppManifest::parse(&rendered["api.yaml"]).unwrap();
        let ingress = m.ingress.unwrap();
        assert_eq!(ingress.host.as_deref(), Some("api.zpevnik.majksa.net"));
        assert!(
            ingress.domains.is_empty(),
            "custom domains dropped for non-prod"
        );
        assert_eq!(ingress.port, 8080);
    }

    #[test]
    fn production_keeps_the_custom_host() {
        let src = sources(&[
            (
                "apps/api/base.yaml",
                "ingress:\n  host: api.example.com\n  port: 8080\n  domains:\n    - www.example.com\n",
            ),
            (
                "apps/api/production.yaml",
                &format!("image: ghcr.io/o/api@{DIGEST}\n"),
            ),
        ]);
        let rendered = render_class(&src, EnvClass::Production, "zpevnik", "majksa.net")
            .unwrap()
            .unwrap();
        let m = AppManifest::parse(&rendered["api.yaml"]).unwrap();
        assert_eq!(
            m.ingress.unwrap().hosts(),
            vec!["api.example.com", "www.example.com"]
        );
    }

    #[test]
    fn port_only_app_is_routed_on_non_production() {
        // The ADR 0013 happy path: no host anywhere, just a port.
        let src = sources(&[
            ("apps/api/base.yaml", "ingress:\n  port: 8080\n"),
            (
                "apps/api/stable.yaml",
                &format!("image: ghcr.io/o/api@{DIGEST}\n"),
            ),
        ]);
        let rendered = render_class(&src, EnvClass::Stable, "zpevnik", "majksa.net")
            .unwrap()
            .unwrap();
        let m = AppManifest::parse(&rendered["api.yaml"]).unwrap();
        assert_eq!(
            m.ingress.unwrap().host.as_deref(),
            Some("api.zpevnik.majksa.net")
        );
    }

    #[test]
    fn name_mismatch_is_rejected() {
        let src = sources(&[
            ("apps/api/base.yaml", "name: other\n"),
            (
                "apps/api/stable.yaml",
                &format!("image: ghcr.io/o/api@{DIGEST}\n"),
            ),
        ]);
        assert!(render_class(&src, EnvClass::Stable, "proj", "majksa.net")
            .unwrap_err()
            .to_string()
            .contains("does not match"));
    }
}
