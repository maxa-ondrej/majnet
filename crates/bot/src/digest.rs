//! Digest bumps (§11.4): a GHCR container publish for `<org>/<app>` becomes a
//! commit on the project ops repo `main`, updating the `image:` digest in
//! `apps/<app>/stable.yaml`. Commits go through the contents API, so GitHub
//! signs them as the App (verified). The production digest moves only via the
//! promote action (phase 4); the review gate lives downstream in the
//! `env/production` render PR.

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
    let app = pkg["name"]
        .as_str()
        .context("package payload has no name")?;
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

    // Builds of main move stable; pr-<N> builds feed the ephemeral flow.
    if let Some(pr) = tag.strip_prefix("pr-").and_then(|n| n.parse::<u64>().ok()) {
        let image = format!("ghcr.io/{org}/{app}@{digest}");
        return crate::ephemeral::on_pr_build(state, org, app, pr, &image).await;
    }

    let image = format!("ghcr.io/{org}/{app}@{digest}");
    tracing::info!(org, app, %image, "bumping stable digest");
    bump_stable_digest(state, org, app, &image).await?;
    state
        .store
        .log_event("digest-bump", Some(org), &format!("{app} → {digest}"))?;
    Ok(())
}

async fn bump_stable_digest(state: &AppState, org: &str, app: &str, image: &str) -> Result<()> {
    let client = state.github.org_client(org).await?;
    let path = format!("apps/{app}/stable.yaml");
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
            (String::from_utf8(decoded)?, Some(item.sha))
        }
        Err(octocrab::Error::GitHub { source, .. }) if source.status_code == 404 => (
            format!("# stable overlay for {app} — digest managed by the bot\nimage: {image}\n"),
            None,
        ),
        Err(e) => return Err(e).context("fetching stable overlay"),
    };

    let updated = replace_image_line(&current, image)?;
    if updated == current && sha.is_some() {
        tracing::info!(org, app, "digest unchanged, nothing to commit");
        return Ok(());
    }

    let short = image
        .rsplit(':')
        .next()
        .map(|d| &d[..12.min(d.len())])
        .unwrap_or("?");
    let message = format!("chore({app}): bump stable digest to {short}");
    match sha {
        Some(sha) => {
            repos
                .update_file(&path, &message, &updated, &sha)
                .branch("main")
                .send()
                .await?;
        }
        None => {
            repos
                .create_file(&path, &message, &updated)
                .branch("main")
                .send()
                .await?;
        }
    }
    Ok(())
}

/// Replace the value of the top-level `image:` line, leaving everything else
/// (comments, other keys) untouched. Appends the key if absent.
pub(crate) fn replace_image_line(content: &str, image: &str) -> Result<String> {
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
    use super::replace_image_line;

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
}
