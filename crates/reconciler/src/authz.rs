//! Authorization plumbing for the reconciler's human-facing endpoints
//! (restart, TTL extend). Role logic + header trust model live in
//! `majnet_common::authz`; config comes from bot snapshots. Projects are
//! addressed by name here — the registry maps name → org for the ops fetch.

use anyhow::{Context, Result};
use axum::http::HeaderMap;
use majnet_common::authz::{self, Actor};
use majnet_common::platform::{PeopleFile, ProjectsFile};
use majnet_common::project::{ProjectConfig, Role};

use crate::AppState;

/// Enforce that the caller is a platform admin (or WG-mesh infra when there's
/// no identity header). For platform-level writes like alert settings.
pub async fn require_platform_admin(state: &AppState, headers: &HeaderMap) -> Result<String> {
    let Some(login) = headers
        .get("tailscale-user-login")
        .and_then(|v| v.to_str().ok())
    else {
        return Ok("infra".into());
    };
    let platform = crate::snapshot::fetch(
        &state.http,
        &state.config,
        &state.config.root_org,
        "platform",
        "main",
    )
    .await?
    .context("platform snapshot unavailable for authz")?;
    let people = PeopleFile::parse(
        platform
            .files
            .get("people.yaml")
            .context("platform repo has no people.yaml")?,
    )?;
    match authz::identify(Some(login), &people)? {
        Actor::Human {
            github,
            platform_admin: true,
        } => Ok(github),
        _ => anyhow::bail!("platform admin required"),
    }
}

/// Enforce `min_role` on `project` for this request; returns the audit label.
pub async fn require(
    state: &AppState,
    headers: &HeaderMap,
    project: &str,
    min_role: Role,
) -> Result<String> {
    let Some(login) = headers
        .get("tailscale-user-login")
        .and_then(|v| v.to_str().ok())
    else {
        // No identity header = WG-mesh infra / node-local break-glass.
        return Ok("infra".into());
    };

    let platform = crate::snapshot::fetch(
        &state.http,
        &state.config,
        &state.config.root_org,
        "platform",
        "main",
    )
    .await?
    .context("platform snapshot unavailable for authz")?;
    let people = PeopleFile::parse(
        platform
            .files
            .get("people.yaml")
            .context("platform repo has no people.yaml")?,
    )?;
    let actor = authz::identify(Some(login), &people)?;

    let project_cfg: Option<ProjectConfig> = match &actor {
        Actor::Human {
            platform_admin: false,
            ..
        } => {
            let projects = ProjectsFile::parse(
                platform
                    .files
                    .get("projects.yaml")
                    .context("platform repo has no projects.yaml")?,
            )?;
            let org = &projects
                .projects
                .iter()
                .find(|p| p.name == project)
                .with_context(|| format!("unknown project {project}"))?
                .org;
            let ops = crate::snapshot::fetch(&state.http, &state.config, org, "ops", "main")
                .await?
                .with_context(|| format!("{org}/ops snapshot unavailable"))?;
            Some(
                serde_yaml::from_slice(
                    ops.files
                        .get("project.yaml")
                        .with_context(|| format!("{org}/ops has no project.yaml"))?,
                )
                .context("parsing project.yaml")?,
            )
        }
        _ => None,
    };

    authz::require_role(&actor, project_cfg.as_ref(), min_role)?;
    Ok(actor.label().to_string())
}
