# 0016 — In-dashboard node terminal

**Status:** accepted · **Date:** 2026-07-17 · relates to [0002](0002-blue-green-via-healthcheck-gated-routing.md), [0004](0004-setup-service-auto-provisioning.md)

## Context

Operators occasionally need an interactive shell — into a running app container
to debug it, or on a node's host to triage an incident the declarative flow
can't express. Today the only path is SSHing to the node by hand. Bringing a
terminal into the dashboard is convenient, but it is the **largest imperative
escape hatch** in the system: arbitrary code execution on production nodes, and
a second exception to "writes go through git" (§6) beyond restart (§16).

What the architecture already provides:

- The **reconciler** is the only component touching node Docker APIs (bollard
  over WireGuard + mTLS) and already runs one-shot `create_exec`/`start_exec`
  (`info.rs`, `db.rs`). bollard 0.21's `StartExecResults::Attached` exposes a
  full bidirectional `input`/`output` duplex plus `resize_exec` — everything an
  interactive TTY needs.
- The reconciler holds **only** Docker mTLS certs + age keys — no SSH. It can,
  however, already root any node through the Docker API (`docker run
  --privileged`), so it is effectively node-root today.
- Human identity reaches the backends as the `Tailscale-User-Login` header,
  injected by `tailscale serve` and role-checked against `people.yaml`
  (`authz::require_platform_admin`). A header-less WG request authenticates as
  `Infra` and passes every gate.

## Decision

**Add an interactive terminal served by the reconciler over a WebSocket,
platform-admin-only and fully audited, with two modes.**

- **Container exec** — `create_exec`/`start_exec` with `attach_stdin` + `tty`
  into a running **app** container; `resize_exec` on TTY resize. Scope is
  **app containers only** — control-plane containers (bot, reconciler,
  edge/Traefik) and database engines are excluded. (Fast-follow: a "debug"
  toggle that attaches a digest-pinned toolbox image — e.g. `netshoot` — sharing
  a shell-less app's namespaces.)
- **Host shell** — a **digest-pinned, minimal privileged helper** container
  (`--privileged --pid=host`) running
  `nsenter -t 1 -m -u -i -n -p -- bash -l` → host root. This reuses the existing
  Docker mTLS credential (**no new credential class**). SSH was rejected: it
  adds an internet-exposed secret (port 22 is public on nodes; the Docker API is
  WG-only) for no capability gain (the reconciler is already node-root via
  Docker). Nodes stay **minimal** — the shell is whatever the host ships (plain
  bash); "least privilege" is explicitly not a goal (platform-admin = root is
  accepted).
- **Transport** — enable axum's `ws` feature + a new terminal route in the
  reconciler; `dashboard/nginx.conf` forwards `Upgrade`/`Connection`
  (`proxy_http_version 1.1`, long read timeout) on the terminal location; the
  frontend adds xterm.js and a WebSocket (the dashboard's first).
- **Auth** — `require_platform_admin`, but the terminal route **does not honor
  the header-less `Infra` bypass**: it requires a resolved *named* human admin,
  so every session is attributable. Identity is the `Tailscale-User-Login`
  header. The `tailscale serve` path (`http://majksa`) injects it directly; the
  public Caddy edge (`dash.majksa.net`) does not, so it uses Caddy's built-in
  **`forward_auth`** against the bot's **`/tsauth`** endpoint, which resolves the
  caller's tailnet IP → user via the Tailscale API (the bot owns that
  credential) and returns the header for Caddy to inject. Caddy strips any
  client-supplied identity header first, so the value is authoritative.

  > **Credential source (updated):** the Tailscale credential `/tsauth` uses is
  > configured from the dashboard **Settings → Tailnet identity** (platform-admin),
  > not hand-edited into `bot.env`. It is a self-renewing **OAuth client**: the bot
  > mints short-lived API tokens from a long-lived client secret on demand and
  > caches them, so nothing needs manual rotation. Stored in the bot's `config`
  > table (DB-first, env fallback — same model as the GHCR token, ADR 0012); a
  > legacy raw `MAJNET_TAILSCALE_API_KEY` is still honored. A **Verify identity**
  > action exercises the credential and resolves the caller live.
  >
  > **WebSocket caveat:** Caddy's `forward_auth` injects the identity header on
  > normal requests but **not on WebSocket upgrades**, so the terminal WS reached
  > the reconciler unauthenticated (403). The dashboard's nginx resolves identity
  > itself for the terminal location via `auth_request` → the bot's `/tsauth`
  > (keyed on the forwarded tailnet IP), preferring an already-injected header so
  > the `tailscale serve` path is unchanged. It uses a `map` (evaluated lazily,
  > after `auth_request`); an `if`/`set` runs in the earlier rewrite phase and
  > would capture an empty value.

  ```caddy
  dash.majksa.net {
      tls /etc/caddy/certs/dash.crt /etc/caddy/certs/dash.key
      request_header -Tailscale-User-Login          # strip client-supplied identity
      forward_auth 127.0.0.1:8080 {                  # bot public listener (localhost)
          uri /tsauth
          copy_headers Tailscale-User-Login
      }
      reverse_proxy 127.0.0.1:8090
  }
  ```
- **Audit** — a **full I/O transcript** (input + output) is recorded per session
  (reconciler store), plus an `events` row on open and close (mirroring the
  `restart` "imperative" event, `state_api.rs`), surfaced in the Activity feed.
- **Production guardrail** — when the target is a production-zone node or
  container, the UI requires a **typed confirmation** and shows a **persistent
  warning banner**, on top of the platform-admin gate.

## Consequences

- This is a deliberate, second imperative escape hatch — the most powerful one
  (RCE on prod). It is justified by real incident-triage need and contained by:
  platform-admin **and named-human** only (no `Infra` bypass), full-transcript
  audit, production confirm + banner, app-containers-only exec, and a
  digest-pinned helper.
- **Host shell = full node root for any platform-admin.** Accepted: platform
  admins can already SSH to nodes and the reconciler is already node-root via
  Docker; the terminal changes convenience, not the trust boundary.
- **No new credential**; the reconciler's Docker mTLS is reused. Nodes are
  unchanged and stay minimal. The privileged helper image (default
  `debian:bookworm-slim`, public) is **pulled on demand** if a node doesn't have
  it — no manual pre-pull; pin it by digest for production.
- **Transcripts can contain secrets** typed or printed during a session, so the
  transcript store is access-controlled (platform-admin read only) and gets a
  defined retention — a new sensitive data class to protect.
- New surface area to maintain: axum `ws`, an nginx upgrade path, xterm.js, and a
  privileged helper container. A privileged `--pid=host` container is itself a
  footgun; sessions must reap it on close.
- Not a state change, so not a commit — it lives beside `restart` as an audited
  imperative action, never as git state.
- If Docker on a node is down, the terminal is unavailable (consistent — all
  reconciler↔node ops are); true break-glass remains an operator's own SSH.

## Open items for implementation
- Transcript storage shape + retention policy (DB table vs file; how long).
- Helper image: pulled on demand + **digest-pinned** (done — `debian:bookworm-slim@sha256:…`, the image invariant; override via `MAJNET_TERM_HELPER_IMAGE`).
- Idle/absolute session timeouts: **done** — the WS bridge auto-closes after 15 min idle (reset on any I/O) or 4 h total, writing a notice into the terminal first, so a forgotten privileged host shell can't linger.

## Notes
- The shell runs **interactive** (`bash -i`, host `bash -il`, else `sh -i`) with a
  real TTY, so it shows a prompt and streams errors. An earlier
  `exec bash 2>/dev/null` fallback accidentally sent the shell's own stderr to
  `/dev/null`, which made bash non-interactive (no prompt) and swallowed errors.
