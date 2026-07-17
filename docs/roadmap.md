# Roadmap

Phased plan from the design doc (§19), tracked here as the implementation progresses.

## Phase 0 — Foundations 🚧 (current)

Tooling ✅ / infra provisioning ⏳:

- [x] Node bootstrap tooling: WireGuard mesh, Docker APIs bound to WG IPs + mTLS, node roles, PKI (`bootstrap/`)
- [x] Firewall tooling: nftables per role, prod 80/443 from Cloudflare ranges w/ weekly refresh (`bootstrap/steps/40`)
- [x] `edge-main` Traefik + hello-world manifests (`platform-seed/platform/`)
- [x] Platform repo seed: nodes.yaml, people.yaml, projects.yaml, ACL template (`platform-seed/`)
- [ ] Provision the 3 Debian nodes + run bootstrap (needs servers, WG pubkey exchange, Docker PKI distribution)
- [ ] Tailscale org + paste rendered base ACL
- [x] Create root org `majksa-platform` on GitHub (done 2026-07-07, id 300856753 — the one manual §2 step; the wizard's seed step pushes `platform-seed/` as the `platform` repo)
- [ ] Cloudflare: origin cert on prod node, proxied DNS record → hello-world reachable publicly

## Phase 1 — Bot MVP 🚧

Code ✅ / live wiring ⏳:

- [x] GitHub App: JWT auth, per-org installation token cache (`bot/src/github.rs`)
- [x] Webhook server: HMAC verification, delivery dedup, event dispatch (`bot/src/webhooks.rs`)
- [x] Digest bumps: GHCR `registry_package` event → App-signed commit to `apps/<app>/stable.yaml` on ops `main` (ADR 0001, `bot/src/digest.rs`)
- [x] Repo access proxy: `GET /api/snapshot/{org}/{repo}/{branch}` — SHA-cached tarballs on the WG-internal listener (`bot/src/proxy.rs`)
- [x] Reconciler notify on `env/*` + platform pushes (best-effort; drift poll backs it up)
- [x] GHA workflow templates: `rust-service`, `web-app` (test → GHCR by digest)
- [ ] Register the GitHub App (key, webhook secret, events per `crates/bot/README.md`) and deploy the bot to the main node
- [ ] Verify the `registry_package` payload digest path against a real delivery (ADR 0001 caveat)

## Phase 2 — Reconciler MVP 🚧

Code ✅ / live verification ⏳:

- [x] Manifest schema v1 + strict validation + base ⊕ overlay merge (`common/src/{manifest,merge}.rs`)
- [x] Rendering: ops `main` push → full-tree render PRs onto `env/*`; stable auto-merges, production waits for admin review (`bot/src/render.rs`)
- [x] Convergence loop: platform + env snapshots → per-project networks → validate → decrypt → diff → deploy; ~5 min drift poll + `/notify` nudge (`reconciler/src/{converge,main}.rs`)
- [x] Blue-green: migrations → health-gated rollout, old container survives failed deploys (ADR 0002, `reconciler/src/deploy.rs`)
- [x] SOPS decrypt (sops subprocess + class age key) → tmpfs delivery via helper container, ro-mounted at `/run/secrets` (`reconciler/src/secrets.rs`)
- [x] Removed-app GC (deletions only when config gone from git) + SQLite event log tagged with causing commit
- [ ] End-to-end verification against a real node (needs phase 0 infra): render PR → merge → converge → hello-world serving
- [ ] Private GHCR pull auth on nodes (bootstrap-level `docker login`; reconciler stays credential-free)

## Phase 3 — Org management 🚧

Code ✅ / live wiring ⏳:

- [x] Registry-gated discovery: App installed ∧ listed in `projects.yaml`; listed-but-uninstalled logs "pending" (`bot/src/org_sync.rs`)
- [x] Org reconciliation loop (hourly + on config pushes): ops repo + scaffold, app repos from `repo-templates/` with `{{app}}`/`{{org}}` placeholders, archive-on-removal, branch protection (`env/production` review gate, app `main` build check), `admins`/`developers` teams + membership
- [x] Tailscale sync: ACL policy rendered from people.yaml + project members, pushed via API; one-shot tagged auth keys minted for ingresses over the WG-internal API (`bot/src/tailscale.rs`)
- [x] Per-project ingress: Traefik + tailscale sidecar (shared netns, state volume, docker-provider constraint on `majnet.project`) ensured by the reconciler on the private node (`reconciler/src/ingress.rs`)
- [ ] Split DNS for `*.<project>.majksa.net` on the tailnet (Tailscale admin: DNS → split DNS pointing at the project ingress IPs; automate later)
- [ ] Live verification: real org onboarding end-to-end (create org → install App → registry line → repos/teams/ACLs appear)

## Phase 4 — Environment classes 🚧

Code ✅ / live wiring ⏳:

- [x] Promote flow: `POST /api/promote/{org}/{app}` copies the stable digest into the production overlay on ops `main`; the gated `env/production` render PR follows automatically (`bot/src/promote.rs`)
- [x] Ephemeral lifecycle: `pr-N` GHCR build → generated manifest (base ⊕ ephemeral overlay ⊕ PR patch) committed directly onto `env/ephemeral` (ADR 0003) → preview-URL PR comment (updated in place); PR close removes the manifest (`bot/src/ephemeral.rs`)
- [x] Ephemeral GC: 48 h grace after manifest removal, 7 d hard TTL enforced even while a manifest lingers; SQLite tracking (`reconciler/src/gc.rs`)
- [x] Reconciler converges all three classes; `age-production`/`age-stable` class keys already wired (§14)
- [ ] Generate the two class age keys + distribute (`age-keygen`; reconciler `MAJNET_AGE_KEY_DIR`)
- [ ] Live verification: PR → preview URL → close → grace GC observed end-to-end

## Phase 5 — Data & polish 🚧

Code ✅ / remaining ⏳:

- [x] DB provisioning: `database: {engine}` in manifests → logical DB + user on the zone's engine container, deterministic HMAC-derived passwords (no state), engine attached to project network, `DATABASE_URL` injected (`reconciler/src/db.rs`; postgres, mariadb, valkey (ACL user), mongodb)
- [x] Engine platform manifests (`platform-seed/platform/databases/`)
- [x] Backups: nightly dumps → restic → offsite + retention, systemd timer (`bootstrap/steps/60-backups.sh`)
- [x] Restart escape hatch: `POST /api/restart/{project}/{class}/{app}`, audit-logged with Tailscale identity (§16)
- [x] Rollback: `POST /api/rollback/{org}` — revert of ops `main` head, propagates via render PRs
- [x] Dashboard MVP: events + promote/rollback/restart (`dashboard/`)
- [x] Runbooks: node-recovery, bad-deploy, db-break-glass, secret-rotation, restore-test, github-outage
- [x] Valkey + MongoDB provisioning (ACL user / dbOwner user; engines + nightly dumps included)
- [x] Full dashboard: manifest editing (validated, committed by the bot), member management (admin-only), ephemeral TTL extension, role-based authorization from `people.yaml` + `project.yaml` (`common/src/authz.rs`; `tailscale serve` is the identity trust anchor)
- [x] Self-update: control-plane version pinned in the platform repo's `version.yaml` (seeded to the exact installed commit), converged by `majnet-update` on the main node via the bot's `GET /api/platform/version`; break-glass = `majnet-update <ref>` (**ADR 0005**, digest-pinned images **ADR 0008**)
- [x] Standard app endpoints: `/healthz` is the default `health.path`; reconciler scrapes `/info` (build metadata) at deploy time and surfaces it per app/env in the dashboard (§16)
- [x] Dashboard-driven control-plane updates: `/control-plane` page (platform-admin) shows running vs latest and publishes/rolls back the pin via `GET`+`PUT /api/control-plane`; control plane reports its own build at `/info` (CI-baked) → real `converged` signal + live rollout progress bar; `majnet-update` stamp-guarded and polled every ~30s (**ADR 0015**)
- [x] In-dashboard node terminal (**ADR 0016**): platform-admin, audited container exec + host shell (nsenter) over a reconciler WebSocket; per-node/per-app entry points; identity injected at the Caddy edge via the bot's `/tsauth`
- [x] Tailnet identity configurable from Settings: the `/tsauth` credential is set from the dashboard (platform-admin) rather than hand-edited into `bot.env` — a self-renewing **OAuth client** (bot mints short-lived tokens from a long-lived secret, so no manual rotation) stored in the bot's `config` table (DB-first, env fallback — same override model as the GHCR token, ADR 0012), with a **Verify identity** action that resolves the caller live. Legacy raw `MAJNET_TAILSCALE_API_KEY` still honored as a fallback. **ACL management is opt-in** (`ts_manage_acl`, default off): the bot only overwrites the tailnet ACL when explicitly enabled, since the generated tag-based policy would lock out an untagged / manually-managed tailnet
- [x] Settings save UX: per-section save buttons replaced by a single sticky "unsaved changes" bar that commits every changed setting (registry / tailnet identity / alerts) at once, with per-section failure reporting
- [x] Per-app resource limits: `resources: { memory, cpus }` in the manifest → applied to the container's Docker `HostConfig` (memory / nano_cpus); editable in the manifest form, surfaced as usage-vs-limit in `/nodes`
- [ ] First weekly restore test actually performed

## Phase 6 — One-line auto-provisioning (Coolify-style install) 🚧

Code ✅ / live verification ⏳ (architecture: **ADR 0004** — the `majnet-setup` provisioner, a fourth disjoint credential class: SSH enrollment key + PKI CA + wizard token):

- [x] **One-line install on the main node** (`bootstrap/install.sh`): deps + rustup, clone, bootstrap role=main, all key material (PKI CA, age class keys, db-master, enroll key, wizard token), release build, systemd units (bot gated on App credentials existing)
- [x] **Web-based setup wizard** (`crates/setup`, one-time token): GitHub App via the [manifest flow](https://docs.github.com/en/apps/creating-github-apps/registering-a-github-app/registering-a-github-app-from-a-manifest) → bot.env written + bot started; platform repo seeded from `platform-seed/` **committed by the bot** (writes-through-git); public listener closes permanently at /finish
- [x] **Node enrollment through the brain** (`POST /enroll`, wizard + WG-internal): bootstrap payload over SSH (root first contact, `majnet`+sudo after hardening), PKI server certs issued from the CA, WG pubkey collected, peers re-rendered on every node, `nodes.yaml` updated via the bot
- [x] The manual `bootstrap/` scripts remain the underlying payload — setup only executes them; standalone break-glass unchanged
- [x] TLS in front of webhooks + wizard: Caddy on the main node with ACME certs (`MAJNET_DOMAIN` at install; `/webhook` → bot, rest → wizard; firewall admits 80/443 instead of 8080/7600) — **ADR 0006**; domain-less installs keep plain HTTP
- [x] Dashboard deployment: `steps/70-dashboard.sh` (main only — installs Tailscale + compose plugin; after the interactive `tailscale up`, `bootstrap.sh 70` brings up compose + `tailscale serve`)
- [ ] Live verification on a real Debian VPS: install (with `MAJNET_DOMAIN` → real ACME issuance) → wizard → App → seed → enroll 2 workers → hello-world serving → `tailscale up` + dashboard step → first `majnet-update` convergence against the seeded pin

> Origin: requirement added 2026-07-03 — the whole setup must be auto-provisioned like Coolify: one command on the master, continue in the web UI, add nodes by handing the brain SSH access.

## Open questions (design doc §20)

1. Backup target: Backblaze B2 vs Hetzner Storage Box
2. Per-project ingress footprint if projects multiply (full Traefik vs lighter proxy)
3. Reconciler self-update via ops repo vs manual bump
4. Whether `people.yaml` drives Tailscale user invitations or only ACLs
5. GHCR scope: per-org packages (default) vs central registry org
