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
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use majnet_common::platform::PeopleFile;
use majnet_common::project::ProjectConfig;
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::Mutex as AsyncMutex;

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
    let Some(tailnet) = ts_tailnet(state) else {
        tracing::warn!("Tailscale credentials unset — skipping ACL sync");
        return Ok(());
    };
    let bearer = ts_bearer(state).await?;
    let policy = render_acl(people, projects);
    let url = format!("https://api.tailscale.com/api/v2/tailnet/{tailnet}/acl");
    let response = state
        .http
        .post(&url)
        .bearer_auth(&bearer)
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
    let tailnet = ts_tailnet(state).context("Tailscale API not configured on the bot")?;
    anyhow::ensure!(
        project
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
        "invalid project name"
    );
    let bearer = ts_bearer(state).await?;
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
        .bearer_auth(&bearer)
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

// ── identity injection for the Caddy edge (dash.majksa.net) ───────────────────
// `tailscale serve` (the http://majksa path) injects Tailscale-User-Login; the
// public Caddy edge does not. Caddy's built-in `forward_auth` calls `/tsauth`,
// which resolves the client's tailnet IP → its Tailscale user and returns it as
// that header (Caddy copies it upstream). The bot owns the Tailscale credential,
// so this stays inside credential isolation (§6).

struct WhoisCache {
    at: Instant,
    ip_to_login: HashMap<String, String>,
}
static WHOIS: OnceLock<AsyncMutex<Option<WhoisCache>>> = OnceLock::new();
const WHOIS_TTL: Duration = Duration::from_secs(60);

/// `GET /tsauth` — Caddy `forward_auth` target. Always 200 (identity
/// enrichment, not a gate): a resolved client gets a `Tailscale-User-Login`
/// header, an unresolved one gets none (backends then treat it as infra, as
/// before). Caddy strips any client-supplied header before calling this, so the
/// value here is authoritative.
pub async fn tsauth(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let login = match client_ip(&headers) {
        Some(ip) => whois_ip(&state, &ip).await,
        None => None,
    };
    let mut resp = Response::new(axum::body::Body::empty());
    if let Some(login) = login {
        if let Ok(v) = axum::http::HeaderValue::from_str(&login) {
            resp.headers_mut().insert("Tailscale-User-Login", v);
        }
    }
    resp
}

/// The immediate client IP from `X-Forwarded-For` (Caddy sets it to the caller).
fn client_ip(headers: &HeaderMap) -> Option<String> {
    let xff = headers.get("x-forwarded-for")?.to_str().ok()?;
    xff.split(',')
        .next()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Resolve a tailnet IP → its Tailscale user login, via the devices API with a
/// short cache (the map changes rarely; this is hit on every dashboard request).
async fn whois_ip(state: &AppState, ip: &str) -> Option<String> {
    let cell = WHOIS.get_or_init(|| AsyncMutex::new(None));
    let mut guard = cell.lock().await;
    let fresh = guard.as_ref().is_some_and(|c| c.at.elapsed() < WHOIS_TTL);
    if !fresh {
        match fetch_ip_map(state).await {
            Ok(ip_to_login) => {
                *guard = Some(WhoisCache {
                    at: Instant::now(),
                    ip_to_login,
                })
            }
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "tailscale whois refresh failed"),
        }
    }
    guard.as_ref().and_then(|c| c.ip_to_login.get(ip).cloned())
}

async fn fetch_ip_map(state: &AppState) -> Result<HashMap<String, String>> {
    let tailnet = ts_tailnet(state).context("Tailscale API not configured")?;
    let bearer = ts_bearer(state).await?;
    let url = format!("https://api.tailscale.com/api/v2/tailnet/{tailnet}/devices");
    let resp = state.http.get(&url).bearer_auth(&bearer).send().await?;
    let status = resp.status();
    let payload: serde_json::Value = resp.json().await?;
    anyhow::ensure!(
        status.is_success(),
        "Tailscale devices list failed ({status}): {payload}"
    );
    let mut map = HashMap::new();
    for d in payload["devices"].as_array().into_iter().flatten() {
        let Some(user) = d["user"].as_str() else {
            continue;
        };
        for addr in d["addresses"].as_array().into_iter().flatten() {
            if let Some(a) = addr.as_str() {
                map.insert(a.to_string(), user.to_string());
            }
        }
    }
    Ok(map)
}

// ── credentials: OAuth client (self-renewing) or a legacy raw token ───────────
// DB-first (dashboard Settings), env fallback — same override model as the GHCR
// token (ADR 0012). An OAuth client secret is long-lived; the bot mints a
// short-lived API token from it on demand and caches it until near expiry, so
// the credential never needs manual rotation.

enum TsCred {
    OAuth {
        client_id: String,
        client_secret: String,
    },
    Token(String),
}

/// A non-empty config value from the bot's store, if set.
fn cfg(state: &AppState, key: &str) -> Option<String> {
    state
        .store
        .get_config(key)
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
}

/// Resolve the configured Tailscale credential: an OAuth client if both parts
/// are set (DB or env), else a legacy raw access token, else `None` (sync off).
fn ts_cred(state: &AppState) -> Option<TsCred> {
    let id =
        cfg(state, "ts_oauth_client_id").or_else(|| state.config.tailscale_oauth_client_id.clone());
    let secret = cfg(state, "ts_oauth_client_secret")
        .or_else(|| state.config.tailscale_oauth_client_secret.clone());
    if let (Some(client_id), Some(client_secret)) = (id, secret) {
        return Some(TsCred::OAuth {
            client_id,
            client_secret,
        });
    }
    cfg(state, "tailscale_api_key")
        .or_else(|| state.config.tailscale_api_key.clone())
        .map(TsCred::Token)
}

/// The effective tailnet, or `None` when no credential is configured. Defaults
/// to `-` (the identity's default tailnet) when a credential is set but the
/// tailnet is left blank.
fn ts_tailnet(state: &AppState) -> Option<String> {
    ts_cred(state)?;
    Some(
        cfg(state, "tailnet")
            .or_else(|| state.config.tailnet.clone())
            .unwrap_or_else(|| "-".into()),
    )
}

struct OauthToken {
    token: String,
    expires_at: Instant,
    client_id: String,
}
static TS_TOKEN: OnceLock<AsyncMutex<Option<OauthToken>>> = OnceLock::new();

/// Drop any cached OAuth token — call after the client credentials change so the
/// next API call mints against the new secret rather than an old cached token.
pub async fn invalidate_ts_token() {
    let cell = TS_TOKEN.get_or_init(|| AsyncMutex::new(None));
    *cell.lock().await = None;
}

/// A bearer token valid for the Tailscale API: the raw token as-is, or a freshly
/// minted (and cached) OAuth access token.
async fn ts_bearer(state: &AppState) -> Result<String> {
    match ts_cred(state).context("Tailscale API not configured")? {
        TsCred::Token(t) => Ok(t),
        TsCred::OAuth {
            client_id,
            client_secret,
        } => oauth_bearer(state, &client_id, &client_secret).await,
    }
}

/// Mint (or reuse a cached) short-lived access token from an OAuth client via the
/// client-credentials grant. Refreshed 60s before expiry; re-minted if the
/// client id changed.
async fn oauth_bearer(state: &AppState, client_id: &str, client_secret: &str) -> Result<String> {
    let cell = TS_TOKEN.get_or_init(|| AsyncMutex::new(None));
    let mut guard = cell.lock().await;
    if let Some(t) = guard.as_ref() {
        if t.client_id == client_id && t.expires_at > Instant::now() + Duration::from_secs(60) {
            return Ok(t.token.clone());
        }
    }
    let resp = state
        .http
        .post("https://api.tailscale.com/api/v2/oauth/token")
        .form(&[
            ("grant_type", "client_credentials"),
            ("client_id", client_id),
            ("client_secret", client_secret),
        ])
        .send()
        .await
        .context("Tailscale OAuth token request failed")?;
    let status = resp.status();
    let payload: serde_json::Value = resp.json().await?;
    anyhow::ensure!(
        status.is_success(),
        "Tailscale OAuth token exchange failed ({status}): {payload}"
    );
    let token = payload["access_token"]
        .as_str()
        .context("OAuth response has no access_token")?
        .to_string();
    let expires_in = payload["expires_in"].as_u64().unwrap_or(3600);
    *guard = Some(OauthToken {
        token: token.clone(),
        expires_at: Instant::now() + Duration::from_secs(expires_in),
        client_id: client_id.to_string(),
    });
    Ok(token)
}

// ── dashboard Settings: manage the credential + verify it ─────────────────────

#[derive(serde::Serialize)]
pub struct TailscaleStatus {
    /// A credential is configured (never reveals it).
    configured: bool,
    /// `"oauth"` (self-renewing), `"token"` (legacy raw), or `"none"`.
    mode: &'static str,
    /// The tailnet in effect (or the stored value), for display.
    tailnet: Option<String>,
}

/// `GET /api/platform/tailscale` — whether Tailnet identity is configured.
pub async fn tailscale_status(State(state): State<Arc<AppState>>) -> axum::Json<TailscaleStatus> {
    let (configured, mode) = match ts_cred(&state) {
        Some(TsCred::OAuth { .. }) => (true, "oauth"),
        Some(TsCred::Token(_)) => (true, "token"),
        None => (false, "none"),
    };
    let tailnet = cfg(&state, "tailnet").or_else(|| state.config.tailnet.clone());
    axum::Json(TailscaleStatus {
        configured,
        mode,
        tailnet,
    })
}

#[derive(serde::Deserialize)]
pub struct TailscaleReq {
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    client_secret: Option<String>,
    #[serde(default)]
    tailnet: Option<String>,
}

/// `POST /api/platform/tailscale` — set the OAuth client + tailnet (platform
/// admin). Omitted/blank secret fields are left unchanged (as with the GHCR
/// token); the token cache is dropped so the next call uses the new secret.
pub async fn tailscale_set(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Json(req): axum::Json<TailscaleReq>,
) -> Result<String, (StatusCode, String)> {
    let actor = crate::authz::require_platform_admin(&state, &headers)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))?;
    let store = |key: &str, val: &str| {
        state
            .store
            .set_config(key, val)
            .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))
    };
    let mut changed: Vec<&str> = vec![];
    if let Some(id) = req
        .client_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        store("ts_oauth_client_id", id)?;
        changed.push("client id");
    }
    if let Some(sec) = req
        .client_secret
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        store("ts_oauth_client_secret", sec)?;
        changed.push("client secret");
    }
    if let Some(tn) = req
        .tailnet
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        store("tailnet", tn)?;
        changed.push("tailnet");
    }
    if changed.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "no changes provided".into()));
    }
    invalidate_ts_token().await;
    let _ = state.store.log_event(
        "tailscale-set",
        None,
        &format!("{} by {actor}", changed.join(", ")),
    );
    Ok("Tailnet identity saved".to_string())
}

#[derive(serde::Serialize)]
pub struct TailscaleVerify {
    tailnet: String,
    /// Devices visible in the tailnet — proves the credential + `devices:read`.
    devices: usize,
    /// The caller's own resolved login, if their tailnet IP is on file.
    you: Option<String>,
}

/// `POST /api/platform/tailscale/verify` — exercise the credential end-to-end:
/// mint a token, list devices, and (best effort) resolve the caller's identity.
pub async fn tailscale_verify(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<axum::Json<TailscaleVerify>, (StatusCode, String)> {
    crate::authz::require_platform_admin(&state, &headers)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))?;
    let tailnet =
        ts_tailnet(&state).ok_or((StatusCode::BAD_REQUEST, "Tailnet identity not set".into()))?;
    let map = fetch_ip_map(&state)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    let you = client_ip(&headers).and_then(|ip| map.get(&ip).cloned());
    Ok(axum::Json(TailscaleVerify {
        tailnet,
        devices: map.len(),
        you,
    }))
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
