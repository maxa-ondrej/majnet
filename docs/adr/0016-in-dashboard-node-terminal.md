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
  so every session is attributable. The `tailscale serve` funnel + WG bind
  remains the only human path.
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
  unchanged and stay minimal. The privileged helper image is pinned by digest
  (the image invariant) and pre-pulled on nodes.
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
- Helper image choice + digest, and how it's kept pre-pulled on nodes.
- Idle/absolute session timeouts (deferred; not gating v1).
