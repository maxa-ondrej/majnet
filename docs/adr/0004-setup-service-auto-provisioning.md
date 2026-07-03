# 0004 — `majnet-setup`: a fourth component owns auto-provisioning

**Status:** accepted · **Date:** 2026-07-03

## Context

Phase 6 (roadmap) requires Coolify-style onboarding: one command on the main
node → web wizard (GitHub App, Tailscale, root org) → add worker nodes by
giving the platform SSH access. This amends design §2's "one manual step"
list — App registration moves into the wizard via GitHub's [App manifest
flow](https://docs.github.com/en/apps/creating-github-apps/registering-a-github-app/registering-a-github-app-from-a-manifest);
org creation stays manual (GitHub has no API for it).

The powers this needs don't fit either existing component without breaking
the credential-isolation invariant (§6):

- **root SSH to every node** — cannot live in the bot (it would stack on the
  GitHub App + Tailscale keys, creating the single credential that owns
  everything) nor in the reconciler (which is deliberately GitHub-blind and
  already holds age keys + Docker certs).
- The wizard **produces** the bot's own credentials (App id, PEM, webhook
  secret) — it must exist and serve HTTP *before* the bot can run at all,
  and before Tailscale or WireGuard exist.
- Issuing Docker mTLS **server certs** for new nodes needs the PKI **CA
  key**, which today never leaves the operator's laptop (`pki/gen-certs.sh`).

## Decision

A fourth control-plane service, **`majnet-setup`** (`crates/setup`), with its
own credential class — the *provisioner*: **enrollment SSH keypair + PKI CA
key + one-time wizard token**. It holds no GitHub, Tailscale, age, or Docker
client credentials, and it never talks to workload containers. (Custody of
the CA key means it *could* mint itself a client cert — accepted: the CA is
machine-identity root-of-trust, and setup already has root SSH on the same
machines; the powers are equivalent, not additive.)

**Install path** (`bootstrap/install.sh`, the one-liner): clone the repo at a
pinned ref → build release binaries → run the existing `bootstrap/` steps
locally with a generated `role=main` node.env → generate CA + reconciler
client cert, class age keys, `db-master.key`, wizard token → install systemd
units (bot, reconciler, setup) → print the wizard URL
(`http://<public-ip>:7600/?token=…`).

**Wizard** (public listener, one-time token, first run only):
1. Collect root org (pre-created by hand, §2), tailnet + Tailscale API key,
   public endpoint for webhooks.
2. GitHub App manifest flow: auto-submitting form →
   `github.com/organizations/<org>/settings/apps/new` → callback `code` →
   `POST /app-manifests/{code}/conversions` → App id + PEM + webhook secret.
   Setup writes `/etc/majnet/bot.env` + the PEM and restarts the bot.
3. Operator installs the App on the root org (wizard links, then verifies).
4. Seed the `platform` repo: setup reads the bundled seed from the install
   checkout, renders it (age recipients into `.sops.yaml`, main node into
   `nodes.yaml`), and hands the tree to the **bot** to commit — see below.
5. Node enrollment (also available later, WG-internal only): operator gives
   host + role and authorizes setup's SSH pubkey → setup pushes the
   `bootstrap/` payload, renders `node.env` (static WG IP by role, current
   peer set), runs `bootstrap.sh` remotely, captures the node's WG pubkey,
   issues its Docker server cert from the CA, re-renders peers on existing
   nodes, and registers the node in `nodes.yaml` — via the bot.
6. Done → the public listener closes permanently (`/etc/majnet/setup-done`);
   only the WG-internal enrollment API remains.

**Writes still go through git:** setup never touches GitHub. The bot gains
two WG-internal, audit-logged endpoints: `POST /api/platform/seed` (create
the `platform` repo from a posted file tree) and `POST /api/platform/node`
(upsert one entry in `nodes.yaml` on platform `main`). Every enrollment is
therefore a bot-authored commit, same as every other state change.

**Control plane runs as systemd services from source-built binaries**, not
containers: majnet publishes no images of itself yet, and the reconciler
managing the daemon that runs it invites circular failure. Revisit together
with self-update (§20.3).

## Consequences

- Credential isolation gains a fourth disjoint class instead of eroding:
  GitHub+Tailscale (bot) / age+Docker-client (reconciler) / SSH+CA+token
  (setup) / none (dashboard).
- First install compiles Rust on the node (~minutes on a small VPS) — the
  price of no self-hosted image pipeline; acceptable once per platform.
- `bootstrap/` scripts stay the real payload and remain runnable standalone
  (break-glass unchanged); setup only executes them over SSH.
- The wizard's public listener is a one-shot attack surface, mitigated by
  the single-use token, and gone after setup completes.
- Webhooks start on plain HTTP behind the token-less public port; fronting
  them with Cloudflare/TLS is follow-up work (tracked in the roadmap).
