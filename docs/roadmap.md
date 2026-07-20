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
- [x] Resource caps on reconciler-managed platform containers: edge-main (Traefik, 256M/0.5cpu) and the managed DB engines (postgres/mariadb/mongodb 1G/1cpu, valkey 512M/0.5cpu) get memory + nano_cpus HostConfig limits, folded into their config-hash so a cap change recreates them (complements the per-app `resources` limits)
- [x] Terminal hardening (**ADR 0016**): the host-shell helper image is digest-pinned (`debian:bookworm-slim@sha256:…`, override via `MAJNET_TERM_HELPER_IMAGE`); WS sessions auto-close after 15 min idle (reset on any I/O) or 4 h total, so a forgotten privileged host shell can't linger
- [x] Terminal container picker: the Terminal page can now pick a container-exec target directly (project → app → environment, environments derived from running containers) instead of only via each app's Exec button
- [x] Managed Adminer (**ADR 0014** phase 2, partial): the reconciler now owns the Adminer container (`platform.rs::converge_adminer`) — `adminer:5` on a private `majnet-admin` network shared with postgres, capped (256M/0.5cpu), config-hash managed; replaces a hand-deployed orphan. Retired the phase-0 `hello-world` smoke test. Follow-up: tailnet routing + auto-login (phase 2 remainder)
- [x] Per-container metrics history (**ADR 0017** follow-up): `container_samples` table (node+container keyed, same tiered rollups), sampler writes each container per tick, `GET /api/metrics/container-history?container=`, per-container CPU sparklines in the `/nodes` container table under the time-range selector
- [x] Metrics-history persistence (**ADR 0017**): a reconciler sampler loop writes node/host samples to SQLite every 60s and compacts them into RRD-style tiers (≤24h raw / 24h–7d 30-min / 7d–30d 1h / >30d 1-day); `GET /api/metrics/history?range=` serves them; `/nodes` gains a Live/1h/6h/24h/7d/30d selector feeding `MetricChart`, and the home fleet widget shows 6h sparklines. Node/host-level (per-container = follow-up)
- [x] Customizable home/overview dashboard at `/` (Projects list moved to `/projects`): an at-a-glance grid of widgets — stat tiles (projects/apps/containers/nodes), fleet health (per-node CPU/MEM meters), deployments in flight, control plane (admin), recent activity — assembled from existing hooks. **Fully customizable**: a "Customize" edit mode with drag-to-reorder + resize (react-grid-layout) and per-widget hide/show; layout persists **per user** server-side (`GET`/`PUT /api/platform/dashboard-layout`, keyed by Tailscale login, in the bot's `config` table). Admin-only widgets hidden for members
- [x] Home-dashboard alerts tile: an Alerts widget showing Discord webhook state + a live "N nodes over threshold" readout (from useAlertSettings + useNodeMetrics, no new backend)
- [x] Shared versioning — platform-cut releases (**ADR 0009**): the bot computes the next semver and tags the app repo's `main` (`POST /api/releases/{org}/{app}/cut?bump=`). `bump=patch|minor|major` explicit, or **`bump=auto`** (option 2) which derives the bump from conventional-commit messages since the last release (`feat`→minor, `!`/`BREAKING CHANGE`→major, else patch); "Auto" option in the dashboard Cut-release menu
- [x] Draft releases (**ADR 0009** follow-up): review-gated cuts. On each push to an app repo's `main` the bot prepares a **draft** — the proposed next version + a generated changelog (conventional commits grouped into Breaking/Features/Fixes/Other) — stored per repo (repo-wide for a monorepo) and shown on the dashboard Releases page with editable notes. Submitting (`POST …/draft/submit`, admin) tags the repo and runs the existing cut→CI→record flow; the changelog is persisted and shown per release. Nothing auto-releases — the draft waits for an operator. Endpoints: `GET/DELETE …/draft`, `POST …/draft/refresh`, `PUT …/draft/notes`, `POST …/draft/submit`
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

- [x] Monorepo apps (**ADR 0018**, phase 1): `repo` on `AppDecl` lets one GitHub repository host several apps (shared `repo` = monorepo); nested images `ghcr.io/<org>/<repo>/<app>`, the package-webhook maps the leaf segment → app, and org-sync leaves the shared repo alone (bring-your-own CI). Ops layout unchanged.
- [x] Monorepo apps (**ADR 0018**, phase 2): repo-aware bot operations — cut-release tags `/repos/<org>/<repo>` as one repo-wide `vX.Y.Z` line (last-version + `bump=auto` range computed over the whole repo), PR-preview comments post to the shared repo with a per-app marker, provenance resolves against the repo, and the New-app UI gained an optional "Monorepo repo" field. Rename of a monorepo app is rejected with a clear message.
- [x] Monorepo apps (**ADR 0018**, reusable build CI): `.github/workflows/app-build.yaml` — a `workflow_call` build-tier pipeline BYO-CI monorepos invoke once per app (matrix) to build + push the nested image `ghcr.io/<org>/<repo>/<app>` with build-tier tags (`pr-<N>` → preview, `sha-`/`latest` → testing). No bot change — the existing `registry_package` leaf-app mapping handles nested packages.
- [x] Monorepo apps (**ADR 0018**, phase 3 — rename): renaming a monorepo member now works — the shared repo is left untouched (only a solo app renames its repo), while the ops `apps/<app>/` dir, `project.yaml` name, nested GHCR package (`<repo>/<old>`→`<repo>/<new>`, copied by digest), and nested image pin all follow the new name. `rewrite_manifest_image` matches on the full image base (flat or nested). Caveat: the owner must update the app's name in the monorepo's BYO build CI.
- [x] Monorepo apps (**ADR 0018**, build-CI scaffold): the "Sync templates" action (`template_sync`) now also **seeds** a `build.yaml` matrix caller (one entry per app → reusable `app-build.yaml`) into any monorepo repo that lacks one, via a one-time `monorepo-ci` PR (never overwrites an existing `build.yaml`). Owner adjusts each app's `context`. Completes the ADR 0018 monorepo arc
- [x] Monorepo apps (**ADR 0018**, phase 4 — naming + grouping): monorepo members are named `<repo>-<leaf>` (e.g. `zpevnik-api`) so names stay unique across the project and self-describe in the flat fleet/metrics/deploy views; the image **leaf strips the prefix**, so the image + BYO CI stay at `ghcr.io/<org>/<repo>/<leaf>` (`AppDecl::image_leaf`, and `digest.rs` maps a nested package back to the prefixed app — the inverse). `apps_post` applies the prefix idempotently and the New-app form previews it. The dashboard project page **groups** a repo's members into one labeled card (`repo` field on the app summary), stripping the redundant prefix in the row label. Adopting the prefix on a legacy bare member (`api`→`zpevnik-api`) is a zero-image rename (no package copy / pin / CI change).
- [x] Project-owned "service" apps (**ADR 0021**): run an external prebuilt image + config with no source repo, no CI, and one environment. A service is a manifests-only app tracked in a `project.yaml` `services:` block ({name, exposure}); its `exposure` maps to a class for placement/ingress — `public`→production (prod node, Cloudflare edge, custom domain), `internal`→stable (private node, tailnet auto-host) — so render/converge/secrets/volumes/database are reused with **zero reconciler change**. `POST /api/services/{org}` (`services::create`) scaffolds `apps/<name>/base.yaml` + the single overlay + the services entry; edit/secrets/archive/delete reuse the app paths (archive drops the services entry). Dashboard: "New service" form + a `service · <exposure>` badge. Single image per service (multi-container stacks compose several services on the project network, ADR 0019). Follow-on: hide the class/deploy/releases UI for services in app-detail.
- [x] Intra-project service discovery (**ADR 0019**): each app container gets a stable DNS alias equal to its manifest `name` on the project network (`deploy::container_spec` → `networking_config` aliases), so sibling apps resolve each other by name independent of the volatile `<project>-<app>-<class>-<hash>` container name that blue-green churns. Enables a multi-service app to keep a single public origin behind its own reverse-proxy app (e.g. `sideline`: proxy → server/web/docs, bot → server). `SPEC_VERSION` bumped 2→3 to re-converge the fleet onto the aliased spec.
- [x] Per-app monorepo releases (**ADR 0020**, phase 1): an optional `release:` block on `AppDecl` (GitOps in `project.yaml`, dashboard-written, no PR) opts a monorepo app into **per-app scoped release tags** `@<scope>/<leaf>@vX.Y.Z` (Changesets-style) instead of the repo-wide `vX.Y.Z` line. Cut/draft/provenance become per **release unit** (the app in per-app mode, else the repo; `AppDecl::release_unit` keys drafts, `unit_apps_and_last` the last version); a bulk `POST …/{repo}/cut-repo` cuts every app at once. The reusable `app-release.yaml` gains `leaf`+`version` inputs and template-sync seeds a per-app **release caller** (parses the scoped tag → builds the one app) via a one-time `monorepo-release-ci` PR. The image tag stays the bare version, so the `registry_package` webhook records per-app unchanged; only the git tag is scoped. Backward compatible — no block = repo-wide as before. Amends ADR 0018/0009.
- [x] Per-app monorepo releases (**ADR 0020**, phase 2 — autorelease): `autorelease: patch|auto` + `paths` globs auto-cut an app on a push to `main` that touches its paths (`patch` always patch, `auto` = conventional commits, via the same `do_cut` tag→CI path); opt-in per app, coexists with manual cut, and autorelease units skip the draft. Changed files come from the push payload; matching is gitignore-style (`globset`). `webhooks::changed_paths` → `releases::on_app_main_push` → `try_autorelease`/`paths_match`.

## Open questions (design doc §20)

1. Backup target: Backblaze B2 vs Hetzner Storage Box
2. Per-project ingress footprint if projects multiply (full Traefik vs lighter proxy)
3. Reconciler self-update via ops repo vs manual bump
4. Whether `people.yaml` drives Tailscale user invitations or only ACLs
5. GHCR scope: per-org packages (default) vs central registry org
