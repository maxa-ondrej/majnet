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
- [ ] Create root org `majksa-platform` on GitHub + push `platform-seed/` as the `platform` repo
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

## Phase 2 — Reconciler MVP

- [ ] Manifest schema v1 (`crates/common`)
- [ ] Rendering: base ⊕ overlay → render PRs (bot side)
- [ ] Single-app convergence to private node (bollard over WG)
- [ ] Blue-green: start new → health check → flip Traefik label → stop old
- [ ] SOPS decrypt → tmpfs-mounted secret files

## Phase 3 — Org management

- [ ] Registry-gated discovery (App installed ∧ listed in `projects.yaml`)
- [ ] Org reconciliation loop: repo creation from templates, settings, branch protection, teams, membership, archive-on-removal
- [ ] Tailscale sync: groups, ACLs, per-project ingress auth keys
- [ ] Per-project ingress (Traefik + tailscale sidecar) + Docker networks
- [ ] Split DNS for `*.{project}.majksa.net` on the tailnet

## Phase 4 — Environment classes

- [ ] Production class: promote PRs, `env/production` render-PR review gate, `age-production` key
- [ ] Ephemeral lifecycle: PR-scoped deploys, preview-URL comments, 48 h grace / 7 d hard TTL GC

## Phase 5 — Data & polish

- [ ] DB provisioning (per-project logical DBs/users) + migrations as one-shot containers
- [ ] restic backups + weekly restore tests
- [ ] Dashboard (reads: reconciler state API; writes: bot → commits/PRs)
- [ ] Runbooks (`docs/runbooks/`)
- [ ] Self-update story

## Open questions (design doc §20)

1. Backup target: Backblaze B2 vs Hetzner Storage Box
2. Per-project ingress footprint if projects multiply (full Traefik vs lighter proxy)
3. Reconciler self-update via ops repo vs manual bump
4. Whether `people.yaml` drives Tailscale user invitations or only ACLs
5. GHCR scope: per-org packages (default) vs central registry org
