//! Build-tier digest bumps (ADR 0009, §11.4): a GHCR container publish for
//! `<org>/<app>` becomes a commit on the project ops repo `main`. The image tag
//! selects the tier: `pr-<N>` → ephemeral preview; `vX.Y.Z` → a release,
//! recorded + auto-tracked into `stable` (see `releases::record`); anything
//! else (`latest`, `sha-…`) → `apps/<app>/testing.yaml`. `production` moves
//! only via promote; the review gate lives downstream in the `env/production`
//! render PR.
//!
//! Commits go through the contents API, so GitHub signs them as the App
//! (verified). Overlay-presence is opt-in (matching `render`): an app runs a
//! class only if it commits that overlay, so an absent overlay skips the bump —
//! we never create the overlay on the app's behalf.

use anyhow::{bail, Context, Result};
use base64::Engine;

use crate::AppState;

pub async fn on_package_published(
    state: &AppState,
    org: &str,
    payload: &serde_json::Value,
) -> Result<()> {
    let action = payload["action"].as_str().unwrap_or_default();
    if action != "published" && action != "updated" {
        return Ok(());
    }
    // `registry_package` puts the data under "registry_package"; the legacy
    // event name is "package". Container publishes carry the digest either on
    // the tag metadata or as the version name itself.
    let pkg = if payload["registry_package"].is_object() {
        &payload["registry_package"]
    } else {
        &payload["package"]
    };
    if pkg["package_type"]
        .as_str()
        .unwrap_or_default()
        .to_lowercase()
        != "container"
    {
        return Ok(());
    }
    // The GHCR package name is `<app>` for a solo repo, or `<repo>/<app>` for a
    // monorepo app (nested package). The MajNet app — the ops dir `apps/<app>/`,
    // the manifest name, the runtime name — is always the LAST segment; the full
    // package path is preserved in the pinned image. App names are unique within
    // a project, so the leaf resolves unambiguously.
    let pkg_name = pkg["name"]
        .as_str()
        .context("package payload has no name")?;
    let app = pkg_name.rsplit('/').next().unwrap_or(pkg_name);
    let version = &pkg["package_version"];
    let tag = version["container_metadata"]["tag"]["name"]
        .as_str()
        .unwrap_or_default();
    let digest = version["container_metadata"]["tag"]["digest"]
        .as_str()
        .filter(|d| d.starts_with("sha256:"))
        .or_else(|| {
            version["version"]
                .as_str()
                .filter(|v| v.starts_with("sha256:"))
        })
        .context("package payload carries no sha256 digest")?;

    let image = format!("ghcr.io/{org}/{pkg_name}@{digest}");

    // The image tag selects the tier (ADR 0009): `pr-<N>` → ephemeral preview;
    // `vX.Y.Z` → a release (record it + auto-track stable); anything else
    // (`latest`, `sha-…`) is a main build → testing.
    if let Some(pr) = tag.strip_prefix("pr-").and_then(|n| n.parse::<u64>().ok()) {
        return crate::ephemeral::on_pr_build(state, org, app, pr, &image).await;
    }
    if is_version_tag(tag) {
        tracing::info!(org, app, tag, %image, "release publish — recording");
        return crate::releases::record(state, org, app, tag, &image).await;
    }

    tracing::info!(org, app, %image, "main build — bumping testing digest");
    if bump_class_digest(state, org, app, &image, "testing").await? {
        state.store.log_event(
            "digest-bump",
            Some(org),
            &format!("{app} testing → {digest}"),
        )?;
    }
    Ok(())
}

/// A `vX.Y.Z` release tag: `v` followed by a digit (excludes `latest`, `valkey`,
/// `sha-…`). The full semver shape is validated downstream by the git tag.
pub(crate) fn is_version_tag(tag: &str) -> bool {
    tag.strip_prefix('v')
        .and_then(|rest| rest.chars().next())
        .is_some_and(|c| c.is_ascii_digit())
}

/// Bump the top-level `image:` digest in `apps/{app}/{class}.yaml` on ops `main`.
/// Overlay-presence is opt-in (ADR 0009 phase 5): an absent overlay means the
/// app doesn't run this class, so we skip rather than create it. Returns whether
/// a commit was made (`false` = opted out, or the digest was already current).
pub(crate) async fn bump_class_digest(
    state: &AppState,
    org: &str,
    app: &str,
    image: &str,
    class: &str,
) -> Result<bool> {
    let client = state.github.org_client(org).await?;
    let path = format!("apps/{app}/{class}.yaml");
    let repos = client.repos(org, "ops");

    let existing = repos.get_content().path(&path).r#ref("main").send().await;
    let (current, sha) = match existing {
        Ok(content) => {
            let item = content
                .items
                .into_iter()
                .next()
                .context("empty contents response")?;
            let encoded = item.content.unwrap_or_default().replace(['\n', ' '], "");
            let decoded = base64::engine::general_purpose::STANDARD.decode(encoded)?;
            (String::from_utf8(decoded)?, item.sha)
        }
        Err(octocrab::Error::GitHub { source, .. }) if source.status_code == 404 => {
            tracing::info!(
                org,
                app,
                class,
                "no {class} overlay — app not opted into this class"
            );
            return Ok(false);
        }
        Err(e) => return Err(e).context("fetching class overlay"),
    };

    let updated = replace_image_line(&current, image)?;
    if updated == current {
        tracing::info!(org, app, class, "digest unchanged, nothing to commit");
        return Ok(false);
    }

    let short = image
        .rsplit(':')
        .next()
        .map(|d| &d[..12.min(d.len())])
        .unwrap_or("?");
    let message = format!("chore({app}): bump {class} digest to {short}");
    repos
        .update_file(&path, &message, &updated, &sha)
        .branch("main")
        .send()
        .await?;
    Ok(true)
}

/// Replace the value of the top-level `image:` line, leaving everything else
/// (comments, other keys) untouched. Appends the key if absent.
pub(crate) fn replace_image_line(content: &str, image: &str) -> Result<String> {
    // An empty or `{}`-only overlay (possibly with comments) has no image line to
    // replace — and a flow `{}` can't have `image:` appended after it (that makes
    // two YAML documents, which then fail to parse). Emit a clean image-only
    // overlay, preserving any leading comments.
    let significant: Vec<&str> = content
        .lines()
        .map(str::trim)
        .filter(|t| !t.is_empty() && !t.starts_with('#'))
        .collect();
    if significant.is_empty() || (significant.len() == 1 && significant[0] == "{}") {
        let mut out: Vec<String> = content
            .lines()
            .filter(|l| l.trim_start().starts_with('#'))
            .map(str::to_string)
            .collect();
        out.push(format!("image: {image}"));
        return Ok(out.join("\n") + "\n");
    }

    let mut out = Vec::new();
    let mut replaced = false;
    for line in content.lines() {
        if !replaced && line.starts_with("image:") {
            out.push(format!("image: {image}"));
            replaced = true;
        } else {
            out.push(line.to_string());
        }
    }
    if !replaced {
        if content.contains("image:") {
            // An indented/nested image key we don't understand — refuse to
            // guess (no partial applies, §12).
            bail!("stable overlay has no top-level image key; refusing to edit");
        }
        out.push(format!("image: {image}"));
    }
    Ok(out.join("\n") + "\n")
}

#[cfg(test)]
mod tests {
    use super::{is_version_tag, replace_image_line};

    #[test]
    fn version_tags_are_recognized() {
        assert!(is_version_tag("v1.4.2"));
        assert!(is_version_tag("v2"));
        assert!(is_version_tag("v0.1.0-rc1"));
        assert!(!is_version_tag("latest"));
        assert!(!is_version_tag("valkey"));
        assert!(!is_version_tag("sha-abc123"));
        assert!(!is_version_tag("v"));
        assert!(!is_version_tag("main"));
    }

    #[test]
    fn replaces_top_level_image_preserving_rest() {
        let input = "# managed\nimage: ghcr.io/o/a@sha256:old\nreplicas: 2\n";
        let out = replace_image_line(input, "ghcr.io/o/a@sha256:new").unwrap();
        assert_eq!(
            out,
            "# managed\nimage: ghcr.io/o/a@sha256:new\nreplicas: 2\n"
        );
    }

    #[test]
    fn appends_when_missing() {
        let out = replace_image_line("# empty overlay\n", "ghcr.io/o/a@sha256:new").unwrap();
        assert!(out.ends_with("image: ghcr.io/o/a@sha256:new\n"));
    }

    #[test]
    fn refuses_nested_image_key() {
        assert!(replace_image_line("spec:\n  image: x\n", "y").is_err());
    }

    #[test]
    fn empty_map_overlay_becomes_clean_image_only() {
        // `{}\n\n` must NOT become `{}\nimage: …` (two YAML docs → parse error).
        assert_eq!(
            replace_image_line("{}\n\n", "ghcr.io/o/a@sha256:x").unwrap(),
            "image: ghcr.io/o/a@sha256:x\n"
        );
        // Comments are preserved; the `{}` is dropped.
        assert_eq!(
            replace_image_line("# managed\n{}\n", "img").unwrap(),
            "# managed\nimage: img\n"
        );
        // Both parse as a single document.
        for src in ["{}\n\n", "# c\n{}\n", ""] {
            let out = replace_image_line(src, "img").unwrap();
            assert!(
                serde_yaml::from_str::<serde_yaml::Value>(&out).is_ok(),
                "not single-doc: {out:?}"
            );
        }
    }
}
