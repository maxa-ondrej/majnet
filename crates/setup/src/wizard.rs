//! The first-run wizard: a stepped, single-page flow served as self-contained
//! HTML (no external assets — CSP-safe, works on a bare VPS). Auth is the
//! one-time token (query param, then cookie) — see main.rs middleware.
//!
//! The five steps are sequential; the left rail reflects real state
//! (done / active / locked) and each step's panel POSTs a form that lands
//! back on `/`. Steps are reachable only once their prerequisite is met.

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{Html, Redirect};
use axum::{Form, Json};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::enroll::{self, EnrollRequest};
use crate::github_app;
use crate::state::SetupState;
use crate::AppState;

type PageResult = Result<Html<String>, (StatusCode, Html<String>)>;

fn fail(e: anyhow::Error) -> (StatusCode, Html<String>) {
    (StatusCode::BAD_GATEWAY, Html(error_doc(&format!("{e:#}"))))
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ---------------------------------------------------------------------------
// Step model
// ---------------------------------------------------------------------------

fn configured(s: &SetupState) -> bool {
    s.configured()
}
fn has_app(s: &SetupState) -> bool {
    s.app_id.is_some()
}
fn worker_count(s: &SetupState) -> usize {
    s.nodes.keys().filter(|n| *n != "main").count()
}

/// Is a step reachable given current state?
fn reachable(s: &SetupState, step: u8) -> bool {
    match step {
        1 => true,
        2 => configured(s),
        3 => has_app(s),
        4 | 5 => s.seeded,
        _ => false,
    }
}

/// Is a step's work complete?
fn done(s: &SetupState, step: u8) -> bool {
    match step {
        1 => configured(s),
        2 => has_app(s),
        3 => s.seeded,
        4 => worker_count(s) >= 1,
        _ => false,
    }
}

/// The step to show when none is requested: the first that isn't done.
fn default_step(s: &SetupState) -> u8 {
    if !configured(s) {
        1
    } else if !has_app(s) {
        2
    } else if !s.seeded {
        3
    } else if worker_count(s) == 0 {
        4
    } else {
        5
    }
}

#[derive(Deserialize)]
pub struct IndexQuery {
    #[serde(default)]
    step: Option<u8>,
}

pub async fn index(State(app): State<Arc<AppState>>, Query(q): Query<IndexQuery>) -> Html<String> {
    let state = app.state.lock().await;
    let active = q
        .step
        .filter(|&n| (1..=5).contains(&n) && reachable(&state, n))
        .unwrap_or_else(|| default_step(&state));

    let webhook = app
        .config
        .public_base_url
        .clone()
        .map(|b| format!("{b}/webhook"))
        .unwrap_or_else(|| format!("http://{}:8080/webhook", state.public_host));

    let main = match active {
        1 => panel_basics(&state),
        2 => panel_app(&state, &webhook),
        3 => panel_seed(&state),
        4 => panel_enroll(&state, &enroll::enroll_pubkey(&app.config)),
        _ => panel_finish(&state),
    };
    Html(shell(&state, active, &main))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

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
    #[serde(default)]
    ghcr_token: String,
}

pub async fn configure(
    State(app): State<Arc<AppState>>,
    Form(form): Form<ConfigureForm>,
) -> Result<Redirect, (StatusCode, Html<String>)> {
    let mut state = app.state.lock().await;
    state.root_org = form.root_org.trim().to_string();
    state.public_host = form.public_host.trim().to_string();
    state.tailnet = form.tailnet.trim().to_string();
    if !form.tailscale_api_key.trim().is_empty() {
        state.tailscale_api_key = form.tailscale_api_key.trim().to_string();
    }
    if !form.ghcr_token.trim().is_empty() {
        state.ghcr_token = form.ghcr_token.trim().to_string();
    }
    state.admin_ssh_keys = form.admin_ssh_keys.trim().to_string();
    state.save(&app.config.state_path()).map_err(fail)?;
    enroll::ensure_main_registered(&app.config, &mut state)
        .await
        .map_err(fail)?;
    github_app::write_bot_config(&app.config, &state, None).map_err(fail)?;
    Ok(Redirect::to("/?step=2"))
}

/// Auto-submitting form: the only way to send a manifest is a browser POST.
pub async fn github_start(State(app): State<Arc<AppState>>) -> PageResult {
    let state = app.state.lock().await;
    if !configured(&state) {
        return Err((
            StatusCode::BAD_REQUEST,
            Html(error_doc("Complete step 1 (platform basics) first.")),
        ));
    }
    let manifest = github_app::manifest(&state, app.config.public_base_url.as_deref(), &app.token);
    let body = format!(
        r#"<div class="mid"><div class="panel"><div class="panel-body">
<p class="lede" style="text-align:center">Sending you to GitHub to create the App…</p>
<form id="f" method="post" action="{action}">
  <input type="hidden" name="manifest" value="{manifest}">
  <noscript><div class="actions" style="justify-content:center"><button class="btn primary">Continue to GitHub</button></div></noscript>
</form></div></div></div>
<script>document.getElementById('f').submit()</script>"#,
        action = esc(&github_app::submit_url(&state)),
        manifest = esc(&manifest.to_string()),
    );
    Ok(Html(doc(&body)))
}

#[derive(Deserialize)]
pub struct CallbackQuery {
    code: String,
}

pub async fn github_callback(
    State(app): State<Arc<AppState>>,
    Query(q): Query<CallbackQuery>,
) -> Result<Redirect, (StatusCode, Html<String>)> {
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
    Ok(Redirect::to("/?step=2"))
}

pub async fn seed(
    State(app): State<Arc<AppState>>,
) -> Result<Redirect, (StatusCode, Html<String>)> {
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
            Html(error_doc(&format!(
                "The bot rejected the seed ({status}). {body}"
            ))),
        ));
    }
    state.seeded = true;
    state.save(&app.config.state_path()).map_err(fail)?;
    Ok(Redirect::to("/?step=4"))
}

#[derive(Serialize)]
pub struct EnrollResult {
    pub ok: bool,
    /// Step-by-step enrollment log (shown to the operator either way).
    pub log: String,
}

/// `POST /enroll.json` — JSON node enrollment for the dashboard's Settings UI.
/// Always 200 with `{ ok, log }`: the log is the point even when it fails, so
/// the operator can see exactly which step broke. State is persisted only on a
/// clean run (matching the HTML handler).
pub async fn enroll_json(
    State(app): State<Arc<AppState>>,
    Json(req): Json<EnrollRequest>,
) -> Json<EnrollResult> {
    let mut state = app.state.lock().await;
    match enroll::run(&app.config, &mut state, &app.http, &req).await {
        Ok(log) => {
            let _ = state.save(&app.config.state_path());
            Json(EnrollResult { ok: true, log })
        }
        Err(e) => Json(EnrollResult {
            ok: false,
            log: format!("{e:#}"),
        }),
    }
}

pub async fn enroll_handler(
    State(app): State<Arc<AppState>>,
    Form(req): Form<EnrollRequest>,
) -> PageResult {
    let mut state = app.state.lock().await;
    let role = esc(&req.role);
    let log = enroll::run(&app.config, &mut state, &app.http, &req)
        .await
        .map_err(fail)?;
    state.save(&app.config.state_path()).map_err(fail)?;
    let body = format!(
        r#"<div class="topbar"><div class="brand"><div class="logo"></div>
<div><h1>MajNet setup</h1><p class="sub">Node enrollment — {role}</p></div></div></div>
<div class="panel"><div class="panel-head"><div class="eyebrow good-t">Enrolled</div>
<h2>{role} node is up</h2>
<p class="lede">The node bootstrapped, joined the WireGuard mesh, and was registered in <code>nodes.yaml</code>.</p></div>
<div class="panel-body">
<div class="term"><div class="term-head"><span class="tt">enrollment log</span><span class="l"></span><span class="l"></span></div>
<pre><code>{log}</code></pre></div>
<div class="actions"><a class="btn primary" href="/?step=4">Enroll another node</a>
<a class="btn ghost" href="/?step=5">Finish setup</a></div></div></div>"#,
        role = role,
        log = esc(&log),
    );
    Ok(Html(doc(&body)))
}

pub async fn finish(State(app): State<Arc<AppState>>) -> PageResult {
    let state = app.state.lock().await;
    if !has_app(&state) || !state.seeded {
        return Err((
            StatusCode::BAD_REQUEST,
            Html(error_doc(
                "Finishing needs the GitHub App created and the platform repo seeded.",
            )),
        ));
    }
    std::fs::write(app.config.done_path(), "setup completed\n").map_err(|e| fail(e.into()))?;
    app.shutdown.notify_waiters();
    tracing::info!("setup finished — public wizard listener shutting down");
    let body = r#"<div class="mid"><div class="panel"><div class="panel-head">
<div class="eyebrow good-t">Complete</div><h2>Your control plane is live</h2>
<p class="lede">This public wizard is now closed for good. Node enrollment stays available on the WireGuard-internal API, and day-2 operations live in <code>docs/runbooks/</code>.</p></div>
<div class="panel-body"><div class="banner good"><svg class="ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M20 6 9 17l-5-5"/></svg>
<div>You can close this tab. Manage the platform from the dashboard over Tailscale.</div></div></div></div></div>"#;
    Ok(Html(doc(body)))
}

// ---------------------------------------------------------------------------
// Panels
// ---------------------------------------------------------------------------

fn panel_basics(s: &SetupState) -> String {
    format!(
        r#"<div class="panel"><div class="panel-head"><div class="eyebrow">Step 1 of 5</div>
<h2>Platform basics</h2>
<p class="lede">Identify the root GitHub org and this node, and register the SSH keys that administer every machine. The root org must already exist on GitHub.</p></div>
<div class="panel-body">
<div class="banner warn"><svg class="ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M12 9v4m0 4h.01M10.3 3.9 1.8 18a2 2 0 0 0 1.7 3h17a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0Z"/></svg>
<div><b>Add at least one SSH key.</b> After setup, this node disables password and root SSH login — without a key here you'd be locked out and forced to recover through your provider's console.</div></div>
<form method="post" action="/configure" class="panel-body" style="padding:0;gap:18px">
<div class="row2">
<div class="field"><label for="f-org">Root GitHub org</label>
<input id="f-org" name="root_org" type="text" value="{org}" required>
<span class="hint">Must already exist — org creation is manual.</span></div>
<div class="field"><label for="f-host">Public host of this node</label>
<input id="f-host" name="public_host" type="text" value="{host}" required>
<span class="hint">Domain or IP that GitHub and your browser reach.</span></div></div>
<div class="row2">
<div class="field"><label for="f-net">Tailnet <span class="opt">— optional</span></label>
<input id="f-net" name="tailnet" type="text" value="{net}" placeholder="example.ts.net">
<span class="hint">Enables member VPN access &amp; ACL sync. Add later if unsure.</span></div>
<div class="field"><label for="f-key">Tailscale API key <span class="opt">— optional</span></label>
<input id="f-key" name="tailscale_api_key" type="password" placeholder="{ts_hint}">
<span class="hint">Stored only on this node.</span></div></div>
<div class="field"><label for="f-ghcr">GHCR pull token <span class="opt">— optional</span></label>
<input id="f-ghcr" name="ghcr_token" type="password" placeholder="ghp_… (read:packages)">
<span class="hint">Classic PAT with read:packages so nodes can pull private app images. Also settable later in Settings.</span></div>
<div class="field"><label for="f-ssh">Admin SSH public keys <span class="opt">— one per line</span></label>
<textarea id="f-ssh" name="admin_ssh_keys" spellcheck="false" placeholder="ssh-ed25519 AAAA… you@laptop">{keys}</textarea>
<span class="hint">Authorized for the <code>majnet</code> admin user on this and every enrolled node.</span></div>
<div class="actions"><button class="btn primary" type="submit">Save &amp; continue</button>
<span class="g">You can revise these before finishing.</span></div>
</form></div></div>"#,
        org = esc(&s.root_org),
        host = esc(&s.public_host),
        net = esc(&s.tailnet),
        keys = esc(&s.admin_ssh_keys),
        ts_hint = if s.tailscale_api_key.is_empty() {
            "tskey-api-…"
        } else {
            "•••••• (saved)"
        },
    )
}

fn panel_app(s: &SetupState, webhook: &str) -> String {
    if let Some(slug) = &s.app_slug {
        format!(
            r#"<div class="panel"><div class="panel-head"><div class="eyebrow good-t">Step 2 of 5 · done</div>
<h2>GitHub App</h2>
<p class="lede">The bot authenticates to GitHub as an org-owned App. Credentials went straight to the bot — never shown here.</p></div>
<div class="panel-body">
<div class="banner good"><svg class="ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M20 6 9 17l-5-5"/></svg>
<div><b>App created.</b> <code>{slug}</code> (id {id}). If it isn't installed on <b>{org}</b> yet, do that now.</div></div>
<ul class="summary-list">
<li><span class="k">Webhook URL</span><span class="v">{webhook}</span></li>
<li><span class="k">Events</span><span class="v">push · pull_request · registry_package</span></li></ul>
<div class="actions">
<a class="btn primary" href="https://github.com/apps/{slug}/installations/new" target="_blank" rel="noopener">Install on {org} ↗</a>
<a class="btn ghost" href="/?step=3">Continue to seed →</a></div></div></div>"#,
            slug = esc(slug),
            id = s.app_id.unwrap_or(0),
            org = esc(&s.root_org),
            webhook = esc(webhook),
        )
    } else {
        format!(
            r#"<div class="panel"><div class="panel-head"><div class="eyebrow">Step 2 of 5</div>
<h2>GitHub App</h2>
<p class="lede">Create the bot's GitHub App in one click via GitHub's manifest flow. You'll confirm on GitHub, then land back here.</p></div>
<div class="panel-body">
<div class="banner" style="background:var(--surface-2);border:1px solid var(--border);color:var(--muted)">
<svg class="ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><circle cx="12" cy="12" r="9"/><path d="M12 8h.01M11 12h1v4h1"/></svg>
<div>Be signed into GitHub as an <b>owner of {org}</b> — the App is created under that org. The webhook will be <code>{webhook}</code>.</div></div>
<div class="actions"><a class="btn primary" href="/github/start">Create the GitHub App</a></div></div></div>"#,
            org = esc(&s.root_org),
            webhook = esc(webhook),
        )
    }
}

fn panel_seed(s: &SetupState) -> String {
    if s.seeded {
        return r#"<div class="panel"><div class="panel-head"><div class="eyebrow good-t">Step 3 of 5 · done</div>
<h2>Platform repo seeded</h2><p class="lede">The initial configuration is committed to the platform repo — the reconciler's source of truth.</p></div>
<div class="panel-body"><div class="banner good"><svg class="ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M20 6 9 17l-5-5"/></svg>
<div><b>Seeded.</b> nodes, people, project registry, version pin, and platform manifests are committed.</div></div>
<div class="actions"><a class="btn primary" href="/?step=4">Enroll nodes →</a></div></div></div>"#.to_string();
    }
    format!(
        r#"<div class="panel"><div class="panel-head"><div class="eyebrow">Step 3 of 5</div>
<h2>Seed the platform repo</h2>
<p class="lede">The bot commits your starting configuration to <code>{org}/platform</code> — the single source of truth the reconciler reads. Writes go through git, always.</p></div>
<div class="panel-body">
<p style="margin:0;font-size:13.5px;color:var(--muted)">This creates the first commit with:</p>
<ul class="summary-list">
<li><span class="k">nodes.yaml</span><span class="v">3 trust-zoned nodes</span></li>
<li><span class="k">people.yaml</span><span class="v">member → identity map</span></li>
<li><span class="k">projects.yaml</span><span class="v">project registry</span></li>
<li><span class="k">version.yaml</span><span class="v">control-plane pin</span></li>
<li><span class="k">platform/</span><span class="v">edge, databases, hello-world</span></li></ul>
<form method="post" action="/seed"><div class="actions">
<button class="btn primary" type="submit">Seed platform repo</button>
<span class="g">Safe to re-run — skips if already seeded.</span></div></form></div></div>"#,
        org = esc(&s.root_org),
    )
}

fn panel_enroll(s: &SetupState, enroll_key: &str) -> String {
    let zone = |role: &str| match role {
        "main" => "var(--good)",
        "prod" => "var(--bad)",
        _ => "var(--accent)",
    };
    let mut rows = String::new();
    for role in ["main", "prod", "private"] {
        let (addr, pill) = match s.nodes.get(role) {
            Some(n) => (
                format!(
                    "{} · {}",
                    n.wireguard_ip,
                    if n.public_endpoint.is_empty() {
                        "local".to_string()
                    } else {
                        n.public_endpoint.clone()
                    }
                ),
                if role == "main" {
                    r#"<span class="pill ok">online</span>"#
                } else {
                    r#"<span class="pill ok">enrolled</span>"#
                },
            ),
            None => (
                "not yet enrolled".to_string(),
                r#"<span class="pill wait">optional</span>"#,
            ),
        };
        rows.push_str(&format!(
            r#"<div class="noderow"><span class="zdot" style="background:{c}"></span>
<span class="meta"><div class="nn">{role}</div><div class="na">{addr}</div></span>{pill}</div>"#,
            c = zone(role),
            role = role,
            addr = esc(&addr),
            pill = pill,
        ));
    }
    format!(
        r#"<div class="panel"><div class="panel-head"><div class="eyebrow">Step 4 of 5</div>
<h2>Enroll worker nodes</h2>
<p class="lede">Hand the platform SSH access to a fresh Debian node — it bootstraps the whole machine, joins the WireGuard mesh, and registers itself. One node per role.</p></div>
<div class="panel-body">
<div class="field"><label>1 · Authorize the platform's key on the new node (as root)</label>
<div class="term"><div class="term-head"><span class="tt">run on the new node</span><span class="l"></span><span class="l"></span><button type="button" class="copy" data-copy="ek">Copy</button></div>
<pre><code id="ek"><span class="p"># </span>echo '<span class="a">{key}</span>' &gt;&gt; /root/.ssh/authorized_keys</code></pre></div></div>
<form method="post" action="/enroll" class="panel-body" style="padding:0;gap:16px">
<div class="field"><label>2 · Choose the role</label>
<div class="seg" role="radiogroup" aria-label="Node role">
<label class="segopt"><input type="radio" name="role" value="prod" checked>
<span class="segcard"><span class="nm"><span class="zdot" style="background:var(--bad)"></span>prod</span>
<span class="zn">public zone — edge &amp; production apps</span><span class="ip">wg 10.88.0.2</span></span></label>
<label class="segopt"><input type="radio" name="role" value="private">
<span class="segcard"><span class="nm"><span class="zdot" style="background:var(--accent)"></span>private</span>
<span class="zn">internal zone — stable, ephemeral, dev DBs</span><span class="ip">wg 10.88.0.3</span></span></label></div>
<span class="hint">The role fixes the node's zone and mesh address — no name to pick, no duplicates.</span></div>
<div class="field"><label for="f-sshhost">3 · SSH host of the new node</label>
<input id="f-sshhost" name="ssh_host" type="text" placeholder="203.0.113.2" required></div>
<div class="actions"><button class="btn primary" type="submit">Enroll node</button>
<span class="g">Runs the full bootstrap remotely — a few minutes.</span></div></form>
<hr class="divider">
<label style="font-size:13px;font-weight:560">Nodes</label>
<div class="nodes">{rows}</div>
<div class="banner" style="background:var(--surface-2);border:1px solid var(--border);color:var(--muted)">
<svg class="ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><circle cx="12" cy="12" r="9"/><path d="M12 8h.01M11 12h1v4h1"/></svg>
<div>You can finish with just <b>main</b> + <b>prod</b>. Enrollment stays available over WireGuard, so add <b>private</b> whenever you migrate that server.</div></div>
<div class="actions"><a class="btn ghost" href="/?step=5">Finish setup →</a></div></div></div>"#,
        key = esc(enroll_key),
        rows = rows,
    )
}

fn panel_finish(s: &SetupState) -> String {
    let node_line = {
        let mut have: Vec<&str> = ["main", "prod", "private"]
            .into_iter()
            .filter(|r| s.nodes.contains_key(*r))
            .collect();
        let missing: Vec<&str> = ["prod", "private"]
            .into_iter()
            .filter(|r| !s.nodes.contains_key(*r))
            .collect();
        if have.is_empty() {
            have.push("main");
        }
        let mut line = have.join(" · ");
        if !missing.is_empty() {
            line.push_str(&format!(
                r#" &nbsp;<span style="color:var(--warn)">({} pending)</span>"#,
                missing.join(", ")
            ));
        }
        line
    };
    let sv = |ok: bool, label: &str| {
        if ok {
            format!(r#"<span class="v" style="color:var(--good)">✓ {label}</span>"#)
        } else {
            format!(r#"<span class="v" style="color:var(--warn)">— {label}</span>"#)
        }
    };
    let can_finish = has_app(s) && s.seeded;
    format!(
        r#"<div class="panel"><div class="panel-head"><div class="eyebrow">Step 5 of 5</div>
<h2>Finish setup</h2>
<p class="lede">Close the public wizard. The one-time listener shuts down permanently; node enrollment stays available on the WireGuard-internal API.</p></div>
<div class="panel-body">
<ul class="summary-list">
<li><span class="k">Platform basics</span>{b1}</li>
<li><span class="k">GitHub App</span>{b2}</li>
<li><span class="k">Platform repo</span>{b3}</li>
<li><span class="k">Nodes</span><span class="v">{nodes}</span></li></ul>
<div class="banner warn"><svg class="ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M12 9v4m0 4h.01M10.3 3.9 1.8 18a2 2 0 0 0 1.7 3h17a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0Z"/></svg>
<div>This is one-way — the public listener can't be reopened. Make sure you can SSH in as <code>majnet@{host}</code> before closing.</div></div>
<form method="post" action="/finish"><div class="actions">
<button class="btn primary" type="submit"{dis}>Finish &amp; close wizard</button>
<a class="btn ghost" href="/?step=4">Not yet</a></div></form></div></div>"#,
        b1 = sv(configured(s), "configured"),
        b2 = sv(has_app(s), "installed"),
        b3 = sv(s.seeded, "seeded"),
        nodes = node_line,
        host = esc(&s.public_host),
        dis = if can_finish { "" } else { " disabled" },
    )
}

// ---------------------------------------------------------------------------
// Shell + chrome
// ---------------------------------------------------------------------------

fn shell(s: &SetupState, active: u8, main: &str) -> String {
    let steps = [
        (1u8, "Platform basics", "Org, host, keys"),
        (2, "GitHub App", "Create &amp; install"),
        (3, "Seed platform repo", "Commit initial config"),
        (4, "Enroll nodes", "prod · private"),
        (5, "Finish", "Close the wizard"),
    ];
    let mut rail = String::new();
    for (n, title, sub) in steps {
        let is_done = done(s, n);
        let is_active = n == active;
        let bead = if is_done {
            "✓".to_string()
        } else {
            n.to_string()
        };
        let mut cls = String::from("step");
        if is_done {
            cls.push_str(" done");
        }
        if is_active {
            cls.push_str(" active");
        }
        let inner = format!(
            r#"<span class="node"><span class="bead">{bead}</span></span><span class="label"><span class="t">{title}</span><span class="d">{sub}</span></span>"#
        );
        if reachable(s, n) {
            rail.push_str(&format!(
                r#"<a class="{cls}" href="/?step={n}">{inner}</a>"#
            ));
        } else {
            rail.push_str(&format!(r#"<div class="{cls} locked">{inner}</div>"#));
        }
    }

    let org_chip = if s.root_org.is_empty() {
        String::new()
    } else {
        format!(
            r#"<span class="chip">root org · {}</span>"#,
            esc(&s.root_org)
        )
    };
    let host_chip = if s.public_host.is_empty() {
        r#"<span class="chip"><span class="dot"></span>main node</span>"#.to_string()
    } else {
        format!(
            r#"<span class="chip"><span class="dot"></span>main · {}</span>"#,
            esc(&s.public_host)
        )
    };

    let body = format!(
        r#"<div class="wrap">
<div class="topbar"><div class="brand"><div class="logo" aria-hidden="true"></div>
<div><h1>MajNet setup</h1><p class="sub">Bring your control plane online</p></div></div>
<div class="context">{host_chip}{org_chip}</div></div>
<div class="grid"><nav class="stepper" aria-label="Setup steps">{rail}</nav>
<main>{main}</main></div></div>"#
    );
    doc(&body)
}

fn error_doc(msg: &str) -> String {
    let body = format!(
        r#"<div class="mid"><div class="panel"><div class="panel-head">
<div class="eyebrow bad-t">Something went wrong</div><h2>That step didn't complete</h2></div>
<div class="panel-body"><div class="banner bad"><svg class="ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><circle cx="12" cy="12" r="9"/><path d="M12 8v5m0 3h.01"/></svg>
<div>{msg}</div></div>
<div class="actions"><a class="btn ghost" href="/">← Back to setup</a></div></div></div></div>"#,
        msg = esc(msg)
    );
    doc(&body)
}

fn doc(body: &str) -> String {
    format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>MajNet setup</title><style>{STYLE}</style></head><body>{body}{SCRIPT}</body></html>"#
    )
}

const SCRIPT: &str = r#"<script>
document.querySelectorAll('.copy').forEach(function(b){
  b.addEventListener('click',function(){
    var el=document.getElementById(b.dataset.copy);
    var t=el.innerText.replace(/^#\s*/,'');
    if(navigator.clipboard)navigator.clipboard.writeText(t);
    var o=b.textContent;b.textContent='Copied';setTimeout(function(){b.textContent=o},1200);
  });
});
</script>"#;

const STYLE: &str = r#"
:root{--bg:#f5f7fa;--surface:#fff;--surface-2:#eef1f6;--border:#d8dee8;--border-strong:#c3ccda;
--text:#18202e;--muted:#5c6676;--faint:#97a1b2;--accent:#2f63f6;--accent-soft:#e7eeff;--accent-ink:#fff;
--good:#16955f;--good-soft:#e0f2e9;--warn:#b3730a;--warn-soft:#fbefd8;--bad:#cf3b3b;--bad-soft:#fbe4e4;
--term-bg:#0d1424;--term-text:#c9d3e6;--term-accent:#7fa2ff;--radius:12px;--radius-sm:8px;
--mono:ui-monospace,"SF Mono","JetBrains Mono","Cascadia Code",Menlo,Consolas,monospace;
--sans:ui-sans-serif,system-ui,-apple-system,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;
--shadow:0 1px 2px rgba(20,30,50,.04),0 8px 24px -12px rgba(20,30,50,.12)}
@media (prefers-color-scheme:dark){:root{--bg:#0a0e16;--surface:#111826;--surface-2:#19212f;--border:#253044;
--border-strong:#33415a;--text:#e7ecf3;--muted:#93a0b4;--faint:#62708a;--accent:#5385ff;--accent-soft:#17233f;
--good:#35c184;--good-soft:#12291f;--warn:#e0a545;--warn-soft:#2a2113;--bad:#ef6f6f;--bad-soft:#2c1717;
--term-bg:#060a12;--term-text:#c2cde2;--term-accent:#86a6ff;--shadow:0 1px 2px rgba(0,0,0,.3),0 12px 32px -16px rgba(0,0,0,.6)}}
:root[data-theme="light"]{--bg:#f5f7fa;--surface:#fff;--surface-2:#eef1f6;--border:#d8dee8;--border-strong:#c3ccda;
--text:#18202e;--muted:#5c6676;--faint:#97a1b2;--accent:#2f63f6;--accent-soft:#e7eeff;--good:#16955f;--good-soft:#e0f2e9;
--warn:#b3730a;--warn-soft:#fbefd8;--bad:#cf3b3b;--bad-soft:#fbe4e4;--term-bg:#0d1424;--term-text:#c9d3e6;--term-accent:#7fa2ff;
--shadow:0 1px 2px rgba(20,30,50,.04),0 8px 24px -12px rgba(20,30,50,.12)}
:root[data-theme="dark"]{--bg:#0a0e16;--surface:#111826;--surface-2:#19212f;--border:#253044;--border-strong:#33415a;
--text:#e7ecf3;--muted:#93a0b4;--faint:#62708a;--accent:#5385ff;--accent-soft:#17233f;--good:#35c184;--good-soft:#12291f;
--warn:#e0a545;--warn-soft:#2a2113;--bad:#ef6f6f;--bad-soft:#2c1717;--term-bg:#060a12;--term-text:#c2cde2;--term-accent:#86a6ff;
--shadow:0 1px 2px rgba(0,0,0,.3),0 12px 32px -16px rgba(0,0,0,.6)}
*{box-sizing:border-box}
body{margin:0;background:var(--bg);color:var(--text);font-family:var(--sans);font-size:15px;line-height:1.55;-webkit-font-smoothing:antialiased}
code{font-family:var(--mono);font-size:.9em}
.wrap{max-width:1080px;margin:0 auto;padding:28px 24px 64px}
.mid{max-width:560px;margin:8vh auto 0;padding:0 24px}
.topbar{display:flex;align-items:center;gap:14px;margin-bottom:26px;flex-wrap:wrap}
.brand{display:flex;align-items:center;gap:11px}
.logo{width:34px;height:34px;border-radius:9px;flex:none;border:1px solid var(--border-strong);position:relative;
background:radial-gradient(circle at 30% 30%,var(--accent) 0 3.2px,transparent 3.6px),
radial-gradient(circle at 72% 34%,var(--accent) 0 3.2px,transparent 3.6px),
radial-gradient(circle at 50% 74%,var(--accent) 0 3.2px,transparent 3.6px),var(--accent-soft)}
.brand h1{font-size:17px;font-weight:680;letter-spacing:-.01em;margin:0}
.brand .sub{font-size:12.5px;color:var(--muted);margin:0}
.context{margin-left:auto;display:flex;gap:8px;flex-wrap:wrap}
.chip{font-family:var(--mono);font-size:12px;color:var(--muted);background:var(--surface);border:1px solid var(--border);
padding:5px 10px;border-radius:999px;display:inline-flex;align-items:center;gap:6px}
.chip .dot{width:7px;height:7px;border-radius:50%;background:var(--good);box-shadow:0 0 0 3px var(--good-soft)}
.grid{display:grid;grid-template-columns:232px 1fr;gap:30px;align-items:start}
.stepper{display:flex;flex-direction:column;gap:2px}
.step{display:grid;grid-template-columns:30px 1fr;gap:12px;align-items:start;padding:11px 12px;border-radius:10px;
position:relative;background:transparent;border:1px solid transparent;text-align:left;width:100%;color:inherit;text-decoration:none}
a.step:hover{background:var(--surface-2)}
.step.active{background:var(--surface);border-color:var(--border);box-shadow:var(--shadow)}
.node{position:relative;display:flex;justify-content:center}
.node .bead{width:26px;height:26px;border-radius:50%;display:grid;place-items:center;font-size:12.5px;font-weight:600;
font-family:var(--mono);z-index:1;background:var(--surface);border:1.5px solid var(--border-strong);color:var(--faint)}
.step.done .bead{background:var(--good);border-color:var(--good);color:#fff}
.step.active .bead{border-color:var(--accent);color:var(--accent);box-shadow:0 0 0 4px var(--accent-soft)}
.step:not(:last-child) .node::after{content:"";position:absolute;top:26px;bottom:-13px;left:50%;width:2px;transform:translateX(-50%);background:var(--border);z-index:0}
.step.done .node::after{background:var(--good)}
.step .label{min-width:0}
.step .t{font-size:13.5px;font-weight:570;letter-spacing:-.005em;display:block}
.step .d{font-size:12px;color:var(--muted);display:block}
.step.locked{opacity:.5;cursor:not-allowed}
.panel{background:var(--surface);border:1px solid var(--border);border-radius:var(--radius);box-shadow:var(--shadow);overflow:hidden}
.panel-head{padding:20px 24px 4px}
.eyebrow{font-size:11px;text-transform:uppercase;letter-spacing:.09em;color:var(--accent);font-weight:640}
.eyebrow.good-t{color:var(--good)}.eyebrow.bad-t{color:var(--bad)}
.panel-head h2{margin:6px 0 2px;font-size:20px;font-weight:660;letter-spacing:-.015em;text-wrap:balance}
.panel-head p.lede{margin:4px 0 0;color:var(--muted);max-width:60ch}
.panel-body{padding:18px 24px 24px;display:flex;flex-direction:column;gap:18px}
.field{display:flex;flex-direction:column;gap:6px}
.field>label{font-size:13px;font-weight:560}
.field .hint{font-size:12.5px;color:var(--muted)}
.field .opt{color:var(--faint);font-weight:400}
input[type=text],input[type=password],textarea{font:inherit;color:var(--text);background:var(--bg);
border:1px solid var(--border-strong);border-radius:var(--radius-sm);padding:9px 11px;width:100%}
input::placeholder,textarea::placeholder{color:var(--faint)}
textarea{font-family:var(--mono);font-size:13px;resize:vertical;min-height:76px}
input:focus-visible,textarea:focus-visible,.btn:focus-visible,.copy:focus-visible,.segopt input:focus-visible+.segcard{outline:2px solid var(--accent);outline-offset:2px}
.row2{display:grid;grid-template-columns:1fr 1fr;gap:14px}
.banner{display:flex;gap:11px;padding:12px 14px;border-radius:var(--radius-sm);font-size:13.5px;align-items:flex-start}
.banner .ico{flex:none;width:18px;height:18px;margin-top:1px}
.banner.warn{background:var(--warn-soft);color:color-mix(in srgb,var(--warn) 82%,var(--text));border:1px solid color-mix(in srgb,var(--warn) 35%,transparent)}
.banner.bad{background:var(--bad-soft);color:color-mix(in srgb,var(--bad) 78%,var(--text));border:1px solid color-mix(in srgb,var(--bad) 35%,transparent)}
.banner.good{background:var(--good-soft);color:color-mix(in srgb,var(--good) 80%,var(--text));border:1px solid color-mix(in srgb,var(--good) 35%,transparent)}
.banner b{font-weight:640}
.term{background:var(--term-bg);border-radius:var(--radius-sm);overflow:hidden;border:1px solid rgba(255,255,255,.06)}
.term-head{display:flex;align-items:center;gap:7px;padding:8px 12px;border-bottom:1px solid rgba(255,255,255,.07)}
.term-head .tt{font-family:var(--mono);font-size:11px;color:#7b88a1;letter-spacing:.02em;margin-right:auto}
.term-head .l{width:9px;height:9px;border-radius:50%;background:#313c52}
.copy{font:inherit;font-size:11.5px;cursor:pointer;color:#aeb9d0;background:rgba(255,255,255,.06);border:1px solid rgba(255,255,255,.09);border-radius:6px;padding:3px 9px}
.copy:hover{background:rgba(255,255,255,.12);color:#fff}
.term pre{margin:0;padding:12px 14px;overflow-x:auto}
.term code{font-family:var(--mono);font-size:12.5px;line-height:1.7;color:var(--term-text);white-space:pre}
.term code .p{color:#5f6d8a}.term code .a{color:var(--term-accent)}
.actions{display:flex;gap:10px;align-items:center;flex-wrap:wrap;margin-top:2px}
.btn{font:inherit;font-weight:560;font-size:14px;cursor:pointer;border-radius:var(--radius-sm);padding:9px 16px;
border:1px solid transparent;display:inline-flex;align-items:center;gap:8px;text-decoration:none}
.btn.primary{background:var(--accent);color:var(--accent-ink)}
.btn.primary:hover{background:color-mix(in srgb,var(--accent) 88%,#000)}
.btn.ghost{background:var(--surface);color:var(--text);border-color:var(--border-strong)}
.btn.ghost:hover{background:var(--surface-2)}
.btn[disabled]{opacity:.45;cursor:not-allowed;pointer-events:none}
.g{color:var(--muted);font-size:12.5px}
.seg{display:grid;grid-template-columns:1fr 1fr;gap:8px}
.segopt{position:relative;display:block;cursor:pointer}
.segopt input{position:absolute;inset:0;opacity:0;cursor:pointer;margin:0}
.segcard{display:flex;flex-direction:column;gap:3px;padding:12px 14px;border:1.5px solid var(--border-strong);border-radius:var(--radius-sm);background:var(--bg)}
.segcard .nm{font-weight:600;font-size:14px;display:flex;align-items:center;gap:8px}
.segcard .zn{font-size:12px;color:var(--muted)}
.segcard .ip{font-family:var(--mono);font-size:11.5px;color:var(--faint);margin-top:2px}
.segopt input:checked+.segcard{border-color:var(--accent);background:var(--accent-soft)}
.zdot{width:9px;height:9px;border-radius:50%;display:inline-block}
.nodes{display:flex;flex-direction:column;gap:8px}
.noderow{display:grid;grid-template-columns:auto 1fr auto;gap:12px;align-items:center;padding:11px 14px;border:1px solid var(--border);border-radius:var(--radius-sm);background:var(--bg)}
.noderow .meta{min-width:0}
.noderow .nn{font-weight:570;font-size:13.5px}
.noderow .na{font-family:var(--mono);font-size:12px;color:var(--muted)}
.pill{font-size:11.5px;font-weight:560;padding:3px 9px;border-radius:999px;white-space:nowrap}
.pill.ok{background:var(--good-soft);color:var(--good)}
.pill.wait{background:var(--warn-soft);color:var(--warn)}
.divider{height:1px;background:var(--border);border:0;margin:4px 0}
.summary-list{list-style:none;margin:0;padding:0;display:flex;flex-direction:column;gap:9px}
.summary-list li{display:flex;gap:10px;font-size:13.5px;align-items:baseline}
.summary-list .k{color:var(--muted);min-width:130px}
.summary-list .v{font-family:var(--mono);font-size:12.5px}
@media (max-width:780px){.grid{grid-template-columns:1fr}
.stepper{flex-direction:row;overflow-x:auto;gap:0;padding-bottom:6px}
.step{grid-template-columns:1fr;gap:6px;min-width:120px;text-align:center}
.step .node{justify-content:center}
.step:not(:last-child) .node::after{top:50%;bottom:auto;left:100%;width:999px;height:2px;transform:translateY(-50%)}
.row2,.seg{grid-template-columns:1fr}.context{margin-left:0;width:100%}}
@media (prefers-reduced-motion:no-preference){main .panel{animation:fade .25s ease}
@keyframes fade{from{opacity:0;transform:translateY(4px)}to{opacity:1;transform:none}}}
"#;
