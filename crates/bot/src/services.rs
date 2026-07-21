//! Project-owned **service** apps (ADR 0021): an external Docker image + config
//! with no source repo, no CI, and one environment. A service is a
//! manifests-only app (`apps/<name>/` with a single class overlay) pinned to the
//! class its `exposure` maps to (`public`→`production`/prod node + edge,
//! `internal`→`stable`/private node + tailnet). It is recorded in `project.yaml`'s
//! `services:` block for tracking — org-sync leaves it alone (not in `apps:`, no
//! repo) and the digest webhook never fires for it (no MajNet-built image).
//!
//! Creation writes the `services:` entry + `apps/<name>/base.yaml` + the single
//! exposure-class overlay (reusing `dashboard_api::{scaffold_base,
//! scaffold_and_declare, commit_file}`); everything after — edit image/config,
//! secrets, archive, delete — reuses the normal app paths.

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use std::sync::Arc;

use crate::dashboard_api::{self, bad_gateway, bad_request, ApiError, NewApp};
use crate::AppState;
use majnet_common::project::{Exposure, Role, ServiceDecl};

#[derive(serde::Deserialize)]
pub struct NewService {
    pub name: String,
    pub exposure: Exposure,
    /// Digest-pinned external image (required — a service has no CI to build one).
    pub image: String,
    /// Custom domain (public services only; internal ones get a tailnet auto-host).
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub domains: Vec<String>,
    /// `postgres` | `mariadb` | `valkey` | `mongodb` | none.
    #[serde(default)]
    pub database: Option<String>,
    /// Optional secrets, dotenv blob (`KEY=VALUE` per line), set on the class.
    #[serde(default)]
    pub secrets: String,
}

/// `POST /api/services/{org}` — create a project-owned service (ADR 0021).
/// Admin-gated (a service can be public/prod). Writes the `services:` entry +
/// `apps/<name>/base.yaml` + the single exposure-class overlay; renders +
/// converges via that class (a public service still gates on its `env/production`
/// render PR at deploy time).
pub async fn create(
    State(state): State<Arc<AppState>>,
    Path(org): Path<String>,
    headers: HeaderMap,
    axum::Json(req): axum::Json<NewService>,
) -> Result<String, ApiError> {
    let actor = crate::authz::require(&state, &headers, &org, Role::Admin)
        .await
        .map_err(|e| (axum::http::StatusCode::FORBIDDEN, format!("{e:#}")))?;
    let name = req.name.trim().to_string();
    dashboard_api::check_name(&name)?;

    let class = req.exposure.class();
    // A public service needs a domain to be reachable; internal gets an auto-host.
    if matches!(req.exposure, Exposure::Public) && req.host.trim().is_empty() {
        return Err(bad_request("a public service needs a custom domain (host)"));
    }

    // Build the equivalent manifests-only NewApp: no repo, single exposure class.
    let newapp = NewApp {
        name: name.clone(),
        image: req.image.trim().to_string(),
        host: req.host.trim().to_string(),
        port: req.port,
        domains: req.domains.clone(),
        classes: vec![class.as_str().to_string()],
        database: req.database.clone(),
        template: String::new(),
        repo: None,
        create_repo: false,
        import: None,
    };
    // Validate the manifest (image digest-pinned, ingress, …) before writing.
    dashboard_api::scaffold_base(&newapp).map_err(|e| bad_request(format!("{e:#}")))?;

    // Name must be free across apps + services.
    let mut project = dashboard_api::read_project(&state, &org)
        .await
        .map_err(bad_gateway)?;
    if project.apps.iter().any(|a| a.name == name)
        || project.services.iter().any(|s| s.name == name)
    {
        return Err(bad_request(format!(
            "{name} already exists in this project"
        )));
    }

    // 1. Record the service in project.yaml (tracking; org-sync ignores `services:`).
    project.services.push(ServiceDecl {
        name: name.clone(),
        exposure: req.exposure,
        repo: None,
    });
    dashboard_api::commit_file(
        &state,
        &org,
        "project.yaml",
        &serde_yaml::to_string(&project).map_err(|e| bad_gateway(e.into()))?,
        &format!(
            "project: declare service {name} ({}) via dashboard by {actor}",
            req.exposure.as_str()
        ),
    )
    .await
    .map_err(bad_gateway)?;

    // 2. Scaffold apps/<name>/base.yaml + the single class overlay (declare=false
    //    → not an AppDecl, so org-sync makes/archives no repo for it).
    dashboard_api::scaffold_and_declare(&state, &org, &newapp, &actor, false)
        .await
        .map_err(bad_gateway)?;

    // 3. Optional secrets on the exposure class.
    if !req.secrets.trim().is_empty() {
        crate::migrate::set_app_secrets(&state, &org, &name, class.as_str(), &req.secrets)
            .await
            .map_err(bad_gateway)?;
    }

    state
        .store
        .log_event(
            "service-created",
            Some(&org),
            &format!("{name} ({}) by {actor}", req.exposure.as_str()),
        )
        .map_err(bad_gateway)?;
    Ok(format!(
        "service {name} created ({} → {} class); {}",
        req.exposure.as_str(),
        class.as_str(),
        if matches!(req.exposure, Exposure::Public) {
            "review the env/production render PR to deploy"
        } else {
            "it auto-deploys on render to the private node"
        }
    ))
}
