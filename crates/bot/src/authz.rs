//! Authorization plumbing for the WG-internal write API: resolve the actor
//! from platform snapshots and enforce project roles. The role logic (and
//! the trust model for the `Tailscale-User-Login` header) lives in
//! `majnet_common::authz`.

use anyhow::{Context, Result};
use axum::http::HeaderMap;
use majnet_common::authz::{self, Actor};
use majnet_common::platform::PeopleFile;
use majnet_common::project::{ProjectConfig, Role};

use crate::AppState;

/// Resolve the acting identity from the request header (no role check).
/// `Infra` for a header-less WG-internal / break-glass request, otherwise the
/// human mapped through `people.yaml`.
pub async fn actor(state: &AppState, headers: &HeaderMap) -> Result<Actor> {
    let login = headers
        .get("tailscale-user-login")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    match &login {
        None => Ok(Actor::Infra),
        Some(login) => {
            let (_, tar) =
                crate::proxy::fetch_snapshot(state, &state.config.root_org, "platform", "main")
                    .await
                    .context("platform snapshot for authz")?;
            let files = majnet_common::tarball::untar(&tar)?;
            let people = PeopleFile::parse(
                files
                    .get("people.yaml")
                    .context("platform repo has no people.yaml")?,
            )?;
            authz::identify(Some(login), &people)
        }
    }
}

/// Platform-admin gate for platform-scoped writes (project registry). Infra
/// (header-less WG requests) passes; humans must carry `admin: true` in
/// `people.yaml`. Returns the audit label.
pub async fn require_platform_admin(state: &AppState, headers: &HeaderMap) -> Result<String> {
    let actor = actor(state, headers).await?;
    match &actor {
        Actor::Infra
        | Actor::Human {
            platform_admin: true,
            ..
        } => Ok(actor.label().to_string()),
        Actor::Human { github, .. } => {
            anyhow::bail!("{github} is not a platform admin")
        }
    }
}

/// Enforce `min_role` on `org` for this request; returns the audit label.
pub async fn require(
    state: &AppState,
    headers: &HeaderMap,
    org: &str,
    min_role: Role,
) -> Result<String> {
    let actor = actor(state, headers).await?;

    // Only non-admin humans need the project config for the role check.
    let project: Option<ProjectConfig> = match &actor {
        Actor::Human {
            platform_admin: false,
            ..
        } => {
            let (_, tar) = crate::proxy::fetch_snapshot(state, org, "ops", "main")
                .await
                .with_context(|| format!("{org}/ops snapshot for authz"))?;
            let files = majnet_common::tarball::untar(&tar)?;
            let yaml = files
                .get("project.yaml")
                .with_context(|| format!("{org}/ops has no project.yaml"))?;
            Some(serde_yaml::from_slice(yaml).context("parsing project.yaml")?)
        }
        _ => None,
    };

    authz::require_role(&actor, project.as_ref(), min_role)?;
    Ok(actor.label().to_string())
}
