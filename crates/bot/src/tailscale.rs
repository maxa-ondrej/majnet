//! Tailscale sync (§7, §11.6) — the access network for humans.
//!
//! The bot renders the ACL policy from platform + project config and pushes
//! it via the Tailscale API; it also provisions tagged auth keys for the
//! per-project ingress sidecars (served to the reconciler over the
//! WG-internal API — the reconciler holds no Tailscale credentials).
//!
//! Zone model: admins reach the control plane (`tag:main`); each project's
//! members reach only that project's ingress (`tag:proj-<name>`).

use anyhow::{Context, Result};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use majnet_common::platform::PeopleFile;
use majnet_common::project::ProjectConfig;
use serde_json::json;
use std::sync::Arc;

use crate::AppState;

/// Render the complete ACL policy. Pure — tested without the API.
pub fn render_acl(people: &PeopleFile, projects: &[(String, ProjectConfig)]) -> serde_json::Value {
    let admins: Vec<&str> = people
        .people
        .iter()
        .filter(|p| p.admin)
        .map(|p| p.tailscale.as_str())
        .collect();

    let mut groups = serde_json::Map::new();
    groups.insert("group:admins".into(), json!(admins));

    let mut tag_owners = serde_json::Map::new();
    tag_owners.insert("tag:main".into(), json!(["group:admins"]));

    let mut acls =
        vec![json!({ "action": "accept", "src": ["group:admins"], "dst": ["tag:main:*"] })];

    for (name, project) in projects {
        let members: Vec<&str> = project
            .members
            .iter()
            .filter_map(|m| {
                people
                    .people
                    .iter()
                    .find(|p| p.github.eq_ignore_ascii_case(&m.user))
            })
            .map(|p| p.tailscale.as_str())
            .collect();
        let group = format!("group:proj-{name}");
        let tag = format!("tag:proj-{name}");
        groups.insert(group.clone(), json!(members));
        tag_owners.insert(tag.clone(), json!(["group:admins"]));
        acls.push(json!({ "action": "accept", "src": [group], "dst": [format!("{tag}:*")] }));
        // Admins can reach every project ingress too.
        acls.push(
            json!({ "action": "accept", "src": ["group:admins"], "dst": [format!("{tag}:*")] }),
        );
    }

    json!({ "groups": groups, "tagOwners": tag_owners, "acls": acls })
}

/// Push the rendered policy. Skipped (with a warning) when no API key is set.
pub async fn sync_acl(
    state: &AppState,
    people: &PeopleFile,
    projects: &[(String, ProjectConfig)],
) -> Result<()> {
    let Some((api_key, tailnet)) = ts_credentials(state) else {
        tracing::warn!("MAJNET_TAILSCALE_API_KEY / MAJNET_TAILNET unset — skipping ACL sync");
        return Ok(());
    };
    let policy = render_acl(people, projects);
    let url = format!("https://api.tailscale.com/api/v2/tailnet/{tailnet}/acl");
    let response = state
        .http
        .post(&url)
        .bearer_auth(api_key)
        .json(&policy)
        .send()
        .await?;
    let status = response.status();
    anyhow::ensure!(
        status.is_success(),
        "Tailscale ACL push failed ({status}): {}",
        response.text().await.unwrap_or_default()
    );
    tracing::info!(projects = projects.len(), "Tailscale ACL synced");
    Ok(())
}

/// WG-internal endpoint: mint a tagged, preauthorized auth key for a
/// project's ingress sidecar. Called by the reconciler when it first creates
/// (or recreates) the ingress; keys are single-purpose and short-lived.
pub async fn authkey(
    State(state): State<Arc<AppState>>,
    Path(project): Path<String>,
) -> Result<String, (StatusCode, String)> {
    mint_authkey(&state, &project)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))
}

async fn mint_authkey(state: &AppState, project: &str) -> Result<String> {
    let (api_key, tailnet) =
        ts_credentials(state).context("Tailscale API not configured on the bot")?;
    anyhow::ensure!(
        project
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
        "invalid project name"
    );
    let url = format!("https://api.tailscale.com/api/v2/tailnet/{tailnet}/keys");
    let body = json!({
        "capabilities": { "devices": { "create": {
            "reusable": false,
            "ephemeral": false,
            "preauthorized": true,
            "tags": [format!("tag:proj-{project}")],
        }}},
        "expirySeconds": 3600,
        "description": format!("majnet ingress: {project}"),
    });
    let response = state
        .http
        .post(&url)
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await?;
    let status = response.status();
    let payload: serde_json::Value = response.json().await?;
    anyhow::ensure!(
        status.is_success(),
        "Tailscale key creation failed ({status}): {payload}"
    );
    let key = payload["key"]
        .as_str()
        .context("key response has no 'key'")?
        .to_string();
    state.store.log_event("ts-authkey", None, project)?;
    Ok(key)
}

fn ts_credentials(state: &AppState) -> Option<(&str, &str)> {
    let key = state
        .config
        .tailscale_api_key
        .as_deref()
        .filter(|k| !k.is_empty())?;
    let tailnet = state.config.tailnet.as_deref().filter(|t| !t.is_empty())?;
    Some((key, tailnet))
}

#[cfg(test)]
mod tests {
    use super::render_acl;
    use majnet_common::platform::{PeopleFile, Person};
    use majnet_common::project::{Member, ProjectConfig, Role};

    fn people() -> PeopleFile {
        PeopleFile {
            people: vec![
                Person {
                    github: "maxa-ondrej".into(),
                    tailscale: "ondrej@example.com".into(),
                    admin: true,
                },
                Person {
                    github: "dev1".into(),
                    tailscale: "dev1@example.com".into(),
                    admin: false,
                },
            ],
        }
    }

    fn project(name: &str, users: &[&str]) -> (String, ProjectConfig) {
        (
            name.to_string(),
            ProjectConfig {
                name: name.to_string(),
                members: users
                    .iter()
                    .map(|u| Member {
                        user: u.to_string(),
                        role: Role::Developer,
                    })
                    .collect(),
                apps: vec![],
            },
        )
    }

    #[test]
    fn admins_reach_control_plane_members_reach_their_project_only() {
        let acl = render_acl(&people(), &[project("zpevnik", &["dev1"])]);
        assert_eq!(
            acl["groups"]["group:admins"],
            serde_json::json!(["ondrej@example.com"])
        );
        assert_eq!(
            acl["groups"]["group:proj-zpevnik"],
            serde_json::json!(["dev1@example.com"])
        );
        let acls = acl["acls"].as_array().unwrap();
        assert!(acls
            .iter()
            .any(|r| r["src"][0] == "group:proj-zpevnik" && r["dst"][0] == "tag:proj-zpevnik:*"));
        assert!(acls
            .iter()
            .any(|r| r["src"][0] == "group:admins" && r["dst"][0] == "tag:main:*"));
    }

    #[test]
    fn unknown_github_users_are_dropped_from_groups() {
        let acl = render_acl(&people(), &[project("p", &["ghost-user"])]);
        assert_eq!(acl["groups"]["group:proj-p"], serde_json::json!([]));
    }
}
