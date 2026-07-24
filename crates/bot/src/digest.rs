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

use anyhow::{Context, Result};
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
    // The GHCR package name is `<app>` for a solo repo, or `<repo>/<leaf>` for a
    // monorepo app (nested package). The MajNet app — the ops dir `apps/<app>/`,
    // the manifest name, the runtime name — mirrors the package path: a solo
    // package is the bare `<app>`; a nested `<repo>/<leaf>` maps to the
    // repo-prefixed app `<repo>-<leaf>` (the inverse of `AppDecl::image_leaf`,
    // which strips that prefix to recover the package). The full package path is
    // preserved in the pinned image. App names are unique within a project, so
    // this resolves unambiguously.
    let pkg_name = pkg["name"]
        .as_str()
        .context("package payload has no name")?;
    let app = pkg_name.replace('/', "-");
    let app = app.as_str();
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

/// A release tag: an optional `v` then a digit — matches both `vX.Y.Z` and the
/// bare `X.Y.Z` some CIs emit (e.g. changesets tags the image with the raw
/// package version). Excludes build-tier tags (`latest`, `sha-…`, `pr-…`) and
/// non-version names (`valkey`, `main`), which don't start with a digit. The
/// full semver shape is validated downstream by the git tag.
pub(crate) fn is_version_tag(tag: &str) -> bool {
    tag.strip_prefix('v')
        .unwrap_or(tag)
        .chars()
        .next()
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

    // Env pins carry ONLY the digest: the bare repository is inherited from
    // `base.yaml`, and the overlay's job is to pin this class to a digest.
    let digest =
        digest_of(image).with_context(|| format!("built image is not digest-pinned: {image}"))?;
    let updated = replace_digest_line(&current, digest)?;
    if updated == current {
        tracing::info!(org, app, class, "digest unchanged, nothing to commit");
        return Ok(false);
    }

    let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
    let short = &hex[..12.min(hex.len())];
    let message = format!("chore({app}): bump {class} digest to {short}");
    repos
        .update_file(&path, &message, &updated, &sha)
        .branch("main")
        .send()
        .await?;
    Ok(true)
}

/// The `sha256:…` a pinned image reference carries, if any.
pub(crate) fn digest_of(image: &str) -> Option<&str> {
    image
        .rsplit_once('@')
        .map(|(_, d)| d)
        .filter(|d| d.starts_with("sha256:"))
}

/// The `sha256:…` this overlay pins — from a top-level `digest:` field, else the
/// `@sha256:…` on a combined top-level `image:` line. Used to read the digest a
/// source overlay (e.g. `stable`) is running before copying it elsewhere.
pub(crate) fn overlay_digest(content: &str) -> Option<String> {
    // Top-level keys only. A `digest:` field is authoritative (mirrors
    // `image_ref`), so it wins over a pin on a combined `image:` regardless of
    // line order — scan for the field first, then fall back to the image pin.
    let top_level = |line: &str| line == line.trim_start();
    content
        .lines()
        .filter(|l| top_level(l))
        .find_map(|l| {
            l.strip_prefix("digest:")
                .map(str::trim)
                .filter(|d| d.starts_with("sha256:"))
        })
        .or_else(|| {
            content
                .lines()
                .filter(|l| top_level(l))
                .find_map(|l| l.strip_prefix("image:").and_then(|i| digest_of(i.trim())))
        })
        .map(str::to_string)
}

/// Set the top-level `digest:` pin, preserving comments and every other key. A
/// promotion or build-tier bump pins ONLY the digest: the bare repository is
/// inherited from `base.yaml`, so a stale combined `image: repo@sha256:…` pin in
/// the overlay is demoted to its bare repo (its pin would otherwise be a
/// misleading duplicate — the effective reference always resolves from the
/// `digest`). A bare-repo `image:` override (no pin) is left untouched.
pub(crate) fn replace_digest_line(content: &str, digest: &str) -> Result<String> {
    use majnet_common::manifest::{image_has_pin, strip_pin};
    // An overlay with no significant keys (empty, blank, or a lone `{}`) can't
    // have `digest:` appended after a flow `{}` (that yields two YAML documents)
    // — emit a clean digest-only overlay, preserving any leading comments.
    let significant = content
        .lines()
        .map(str::trim)
        .any(|t| !t.is_empty() && !t.starts_with('#') && t != "{}");
    if !significant {
        let mut out: Vec<String> = content
            .lines()
            .filter(|l| l.trim_start().starts_with('#'))
            .map(str::to_string)
            .collect();
        out.push(format!("digest: {digest}"));
        return Ok(out.join("\n") + "\n");
    }

    let mut out: Vec<String> = Vec::new();
    let mut replaced = false;
    for line in content.lines() {
        let top_level = line == line.trim_start();
        // Demote a stale combined `image:` pin to its bare repository.
        if top_level && line.starts_with("image:") {
            let value = line["image:".len()..].trim();
            out.push(if image_has_pin(value) {
                format!("image: {}", strip_pin(value))
            } else {
                line.to_string()
            });
            continue;
        }
        // Replace the first top-level `digest:`; drop any duplicates.
        if top_level && line.starts_with("digest:") {
            if !replaced {
                out.push(format!("digest: {digest}"));
                replaced = true;
            }
            continue;
        }
        out.push(line.to_string());
    }
    if !replaced {
        out.push(format!("digest: {digest}"));
    }
    Ok(out.join("\n") + "\n")
}

#[cfg(test)]
mod tests {
    use super::{is_version_tag, overlay_digest, replace_digest_line};

    #[test]
    fn version_tags_are_recognized() {
        assert!(is_version_tag("v1.4.2"));
        assert!(is_version_tag("v2"));
        assert!(is_version_tag("v0.1.0-rc1"));
        // Bare (no `v`) versions — changesets tags images with the raw version.
        assert!(is_version_tag("0.38.7"));
        assert!(is_version_tag("1.0.0"));
        assert!(!is_version_tag("latest"));
        assert!(!is_version_tag("valkey"));
        assert!(!is_version_tag("sha-abc123"));
        assert!(!is_version_tag("pr-42"));
        assert!(!is_version_tag("v"));
        assert!(!is_version_tag("main"));
    }

    const D: &str = "sha256:new";

    #[test]
    fn replaces_existing_digest_preserving_rest() {
        let input = "# managed\ndigest: sha256:old\nreplicas: 2\n";
        assert_eq!(
            replace_digest_line(input, D).unwrap(),
            "# managed\ndigest: sha256:new\nreplicas: 2\n"
        );
    }

    #[test]
    fn appends_digest_when_missing() {
        let out = replace_digest_line("# empty overlay\n", D).unwrap();
        assert!(out.ends_with("digest: sha256:new\n"), "{out:?}");
    }

    #[test]
    fn demotes_stale_combined_image_pin_to_bare_repo() {
        // The migration case: a legacy overlay carrying only a combined pin.
        let out = replace_digest_line("image: ghcr.io/o/a@sha256:old\n", D).unwrap();
        assert!(out.contains("image: ghcr.io/o/a\n"));
        assert!(out.contains("digest: sha256:new"));
        assert!(!out.contains("sha256:old"));
    }

    #[test]
    fn keeps_bare_image_override_and_other_keys() {
        let out = replace_digest_line("image: ghcr.io/o/a\ningress:\n  host: x\n  port: 80\n", D)
            .unwrap();
        assert!(out.contains("image: ghcr.io/o/a\n"));
        assert!(out.contains("host: x"));
        assert!(out.contains("digest: sha256:new"));
        serde_yaml::from_str::<serde_yaml::Value>(&out).unwrap();
    }

    #[test]
    fn empty_map_overlay_becomes_clean_digest_only() {
        // `{}` must be dropped (a key can't follow a flow map — two YAML docs).
        assert_eq!(
            replace_digest_line("{}\n\n", D).unwrap(),
            "digest: sha256:new\n"
        );
        assert_eq!(
            replace_digest_line("# managed\n{}\n", D).unwrap(),
            "# managed\ndigest: sha256:new\n"
        );
        for src in ["{}\n\n", "# c\n{}\n", "", "image: ghcr.io/o/a@sha256:old\n"] {
            let out = replace_digest_line(src, D).unwrap();
            assert!(
                serde_yaml::from_str::<serde_yaml::Value>(&out).is_ok(),
                "not single-doc: {out:?}"
            );
        }
    }

    #[test]
    fn overlay_digest_reads_field_or_combined_image() {
        assert_eq!(
            overlay_digest("digest: sha256:abc\n").as_deref(),
            Some("sha256:abc")
        );
        assert_eq!(
            overlay_digest("image: ghcr.io/o/a@sha256:def\n").as_deref(),
            Some("sha256:def")
        );
        // Field wins when both present; nested keys ignored.
        assert_eq!(
            overlay_digest("image: ghcr.io/o/a@sha256:def\ndigest: sha256:abc\n").as_deref(),
            Some("sha256:abc")
        );
        assert_eq!(overlay_digest("spec:\n  digest: sha256:x\n"), None);
        assert_eq!(overlay_digest("image: ghcr.io/o/a\n"), None);
    }
}
