//! Git data API helpers (trees, commits, refs) shared by the render pipeline
//! and org reconciliation. All routes take `repo` as `"/repos/{org}/{name}"`.

use anyhow::{Context, Result};
use serde_json::json;
use std::collections::BTreeMap;

pub async fn get_branch_head(
    client: &octocrab::Octocrab,
    repo: &str,
    branch: &str,
) -> Result<Option<String>> {
    let result: Result<serde_json::Value, _> = client
        .get(format!("{repo}/git/ref/heads/{branch}"), None::<&()>)
        .await;
    match result {
        Ok(r) => Ok(Some(
            r["object"]["sha"]
                .as_str()
                .context("ref has no sha")?
                .to_string(),
        )),
        Err(octocrab::Error::GitHub { source, .. }) if source.status_code == 404 => Ok(None),
        Err(e) => Err(e).context("resolving branch head"),
    }
}

/// Create a complete tree from file contents (no base tree — full snapshot).
pub async fn create_tree(
    client: &octocrab::Octocrab,
    repo: &str,
    files: &BTreeMap<String, String>,
) -> Result<String> {
    let items: Vec<_> = files
        .iter()
        .map(|(path, content)| json!({ "path": path, "mode": "100644", "type": "blob", "content": content }))
        .collect();
    let tree: serde_json::Value = client
        .post(format!("{repo}/git/trees"), Some(&json!({ "tree": items })))
        .await
        .context("creating tree")?;
    Ok(tree["sha"]
        .as_str()
        .context("tree response has no sha")?
        .to_string())
}

/// Incremental tree on top of `base_tree`: `Some(content)` adds/updates a
/// file, `None` deletes it.
pub async fn create_tree_incremental(
    client: &octocrab::Octocrab,
    repo: &str,
    base_tree: &str,
    changes: &BTreeMap<String, Option<String>>,
) -> Result<String> {
    let items: Vec<_> = changes
        .iter()
        .map(|(path, content)| match content {
            Some(content) => {
                json!({ "path": path, "mode": "100644", "type": "blob", "content": content })
            }
            None => json!({ "path": path, "mode": "100644", "type": "blob", "sha": null }),
        })
        .collect();
    let tree: serde_json::Value = client
        .post(
            format!("{repo}/git/trees"),
            Some(&json!({ "base_tree": base_tree, "tree": items })),
        )
        .await
        .context("creating incremental tree")?;
    Ok(tree["sha"]
        .as_str()
        .context("tree response has no sha")?
        .to_string())
}

/// The tree SHA a commit points at.
pub async fn commit_tree(client: &octocrab::Octocrab, repo: &str, commit: &str) -> Result<String> {
    let c: serde_json::Value = client
        .get(format!("{repo}/git/commits/{commit}"), None::<&()>)
        .await?;
    Ok(c["tree"]["sha"]
        .as_str()
        .context("commit has no tree sha")?
        .to_string())
}

pub async fn create_commit(
    client: &octocrab::Octocrab,
    repo: &str,
    tree: &str,
    parents: &[&str],
    message: &str,
) -> Result<String> {
    let commit: serde_json::Value = client
        .post(
            format!("{repo}/git/commits"),
            Some(&json!({ "message": message, "tree": tree, "parents": parents })),
        )
        .await
        .context("creating commit")?;
    Ok(commit["sha"]
        .as_str()
        .context("commit has no sha")?
        .to_string())
}

pub async fn create_ref(
    client: &octocrab::Octocrab,
    repo: &str,
    branch: &str,
    sha: &str,
) -> Result<()> {
    let _: serde_json::Value = client
        .post(
            format!("{repo}/git/refs"),
            Some(&json!({ "ref": format!("refs/heads/{branch}"), "sha": sha })),
        )
        .await
        .with_context(|| format!("creating ref {branch}"))?;
    Ok(())
}

pub async fn force_update_ref(
    client: &octocrab::Octocrab,
    repo: &str,
    branch: &str,
    sha: &str,
) -> Result<()> {
    let _: serde_json::Value = client
        .patch(
            format!("{repo}/git/refs/heads/{branch}"),
            Some(&json!({ "sha": sha, "force": true })),
        )
        .await
        .with_context(|| format!("updating ref {branch}"))?;
    Ok(())
}
