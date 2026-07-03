//! The first-run wizard: one page, sequential steps, plain HTML forms. Auth
//! is the one-time token (query param, then cookie) — see main.rs middleware.

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{Html, Redirect};
use axum::Form;
use serde::Deserialize;
use std::sync::Arc;

use crate::enroll::{self, EnrollRequest};
use crate::github_app;
use crate::AppState;

type PageResult = Result<Html<String>, (StatusCode, String)>;

fn fail(e: anyhow::Error) -> (StatusCode, String) {
    (StatusCode::BAD_GATEWAY, format!("{e:#}"))
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub async fn index(State(app): State<Arc<AppState>>) -> Html<String> {
    let state = app.state.lock().await;
    let done = |on: bool| if on { "✓" } else { "○" };
    let configured = state.configured();
    let has_app = state.app_id.is_some();
    let workers = state.nodes.keys().filter(|n| *n != "main").count();

    let app_step = if let Some(slug) = &state.app_slug {
        format!(
            r#"App <b>{slug}</b> created (id {}). <a href="https://github.com/apps/{slug}/installations/new">Install it on <b>{org}</b></a>, then continue below."#,
            state.app_id.unwrap_or(0),
            org = esc(&state.root_org),
        )
    } else if configured {
        r#"<a href="/github/start">Create the GitHub App</a> (manifest flow — one click on GitHub)."#.into()
    } else {
        "Complete step 1 first.".into()
    };

    let body = format!(
        r#"<h1>MajNet setup</h1>
<p>Prerequisite: the root org (<code>{org_val}</code>) must exist on GitHub — org creation is the one manual step (§2).</p>

<h2>{s1} 1. Platform basics</h2>
<form method="post" action="/configure">
  <label>Root org <input name="root_org" value="{org_val}" required></label><br>
  <label>Public host/IP of this node <input name="public_host" value="{host_val}" required></label><br>
  <label>Tailnet (e.g. example.com) <input name="tailnet" value="{tailnet_val}"></label><br>
  <label>Tailscale API key <input name="tailscale_api_key" type="password" placeholder="{ts_hint}"></label><br>
  <label>Admin SSH pubkeys (one per line)<br><textarea name="admin_ssh_keys" rows="3" cols="70">{keys_val}</textarea></label><br>
  <button>Save</button>
</form>

<h2>{s2} 2. GitHub App</h2>
<p>{app_step}</p>

<h2>{s3} 3. Seed the platform repo</h2>
<form method="post" action="/seed"><button {seed_dis}>Create {org_val}/platform from the seed</button></form>

<h2>{s4} 4. Enroll worker nodes ({workers} enrolled)</h2>
<p>On each fresh Debian node, authorize the enrollment key for root:</p>
<pre>echo '{pubkey}' >> /root/.ssh/authorized_keys</pre>
<form method="post" action="/enroll">
  <label>Role <select name="role"><option>prod</option><option>private</option></select></label>
  <label>Name <input name="name" placeholder="prod"></label>
  <label>SSH host <input name="ssh_host" placeholder="198.51.100.2" required></label>
  <button>Enroll (takes minutes)</button>
</form>

<h2>{s5} 5. Finish</h2>
<form method="post" action="/finish"><button {fin_dis}>Close the wizard (public listener shuts down)</button></form>
<p><small>Node enrollment stays available afterwards on the WG-internal listener.</small></p>"#,
        s1 = done(configured),
        s2 = done(has_app),
        s3 = done(state.seeded),
        s4 = done(workers >= 2),
        s5 = done(false),
        org_val = esc(&state.root_org),
        host_val = esc(&state.public_host),
        tailnet_val = esc(&state.tailnet),
        ts_hint = if state.tailscale_api_key.is_empty() {
            "tskey-api-…"
        } else {
            "(saved)"
        },
        keys_val = esc(&state.admin_ssh_keys),
        seed_dis = if has_app { "" } else { "disabled" },
        fin_dis = if state.seeded { "" } else { "disabled" },
        pubkey = esc(&enroll::enroll_pubkey(&app.config)),
    );
    Html(page(&body))
}

#[derive(Deserialize)]
pub struct ConfigureForm {
    root_org: String,
    public_host: String,
    #[serde(default)]
    tailnet: String,
    #[serde(default)]
    tailscale_api_key: String,
    #[serde(default)]
    admin_ssh_keys: String,
}

pub async fn configure(
    State(app): State<Arc<AppState>>,
    Form(form): Form<ConfigureForm>,
) -> Result<Redirect, (StatusCode, String)> {
    let mut state = app.state.lock().await;
    state.root_org = form.root_org.trim().to_string();
    state.public_host = form.public_host.trim().to_string();
    state.tailnet = form.tailnet.trim().to_string();
    if !form.tailscale_api_key.trim().is_empty() {
        state.tailscale_api_key = form.tailscale_api_key.trim().to_string();
    }
    state.admin_ssh_keys = form.admin_ssh_keys.trim().to_string();
    state.save(&app.config.state_path()).map_err(fail)?;
    enroll::ensure_main_registered(&app.config, &mut state)
        .await
        .map_err(fail)?;
    github_app::write_bot_config(&app.config, &state, None).map_err(fail)?;
    Ok(Redirect::to("/"))
}

/// Auto-submitting form: the only way to send a manifest is a browser POST.
pub async fn github_start(State(app): State<Arc<AppState>>) -> PageResult {
    let state = app.state.lock().await;
    if !state.configured() {
        return Err((StatusCode::BAD_REQUEST, "complete step 1 first".into()));
    }
    let manifest = github_app::manifest(&state, &app.token);
    let body = format!(
        r#"<p>Redirecting to GitHub to create the App…</p>
<form id="f" method="post" action="{action}">
  <input type="hidden" name="manifest" value="{manifest}">
  <noscript><button>Continue to GitHub</button></noscript>
</form>
<script>document.getElementById('f').submit()</script>"#,
        action = esc(&github_app::submit_url(&state)),
        manifest = esc(&manifest.to_string()),
    );
    Ok(Html(page(&body)))
}

#[derive(Deserialize)]
pub struct CallbackQuery {
    code: String,
}

pub async fn github_callback(
    State(app): State<Arc<AppState>>,
    Query(q): Query<CallbackQuery>,
) -> Result<Redirect, (StatusCode, String)> {
    // The token middleware already checked ?token= (GitHub echoes our
    // redirect_url verbatim, token included).
    let creds = github_app::exchange(&app.http, &q.code)
        .await
        .map_err(fail)?;
    let mut state = app.state.lock().await;
    state.app_id = Some(creds.id);
    state.app_slug = Some(creds.slug.clone());
    github_app::write_bot_config(&app.config, &state, Some(&creds)).map_err(fail)?;
    state.save(&app.config.state_path()).map_err(fail)?;
    drop(state);
    github_app::restart_bot().await;
    tracing::info!(app = creds.slug, url = creds.html_url, "GitHub App created");
    Ok(Redirect::to("/"))
}

pub async fn seed(State(app): State<Arc<AppState>>) -> PageResult {
    let mut state = app.state.lock().await;
    enroll::ensure_main_registered(&app.config, &mut state)
        .await
        .map_err(fail)?;
    let seed_dir = app.config.repo_dir.join("platform-seed");
    let files = crate::seed::build_tree(&seed_dir, &state).map_err(fail)?;
    let resp = app
        .http
        .post(format!("{}/api/platform/seed", app.config.bot_url))
        .json(&serde_json::json!({ "files": files }))
        .send()
        .await
        .map_err(|e| fail(e.into()))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("bot rejected seed ({status}): {body}"),
        ));
    }
    state.seeded = true;
    state.save(&app.config.state_path()).map_err(fail)?;
    Ok(Html(page(&format!(
        r#"<p>{}</p><p><a href="/">Back</a></p>"#,
        esc(&body)
    ))))
}

pub async fn enroll_handler(
    State(app): State<Arc<AppState>>,
    Form(req): Form<EnrollRequest>,
) -> PageResult {
    let mut state = app.state.lock().await;
    let log = enroll::run(&app.config, &mut state, &app.http, &req)
        .await
        .map_err(fail)?;
    state.save(&app.config.state_path()).map_err(fail)?;
    Ok(Html(page(&format!(
        r#"<h2>Enrollment log — {}</h2><pre>{}</pre><p><a href="/">Back</a></p>"#,
        esc(&req.name),
        esc(&log)
    ))))
}

pub async fn finish(State(app): State<Arc<AppState>>) -> PageResult {
    let state = app.state.lock().await;
    if state.app_id.is_none() || !state.seeded {
        return Err((
            StatusCode::BAD_REQUEST,
            "finish requires the GitHub App and a seeded platform repo".into(),
        ));
    }
    std::fs::write(app.config.done_path(), "setup completed\n").map_err(|e| fail(e.into()))?;
    app.shutdown.notify_waiters();
    tracing::info!("setup finished — public wizard listener shutting down");
    Ok(Html(page(
        "<p>Setup complete. This public listener is now closed; enrollment \
         remains available on the WG-internal API. Docs: <code>docs/runbooks/</code>.</p>",
    )))
}

fn page(body: &str) -> String {
    format!(
        r#"<!doctype html><meta charset="utf-8"><title>MajNet setup</title>
<style>body{{font:16px/1.5 system-ui;max-width:48rem;margin:2rem auto;padding:0 1rem}}
input,textarea,select{{font:inherit;margin:.2rem 0}}pre{{background:#8882;padding:.6rem;overflow-x:auto}}</style>
{body}"#
    )
}
