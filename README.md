# MajNet v2

A self-hosted deployment platform: **GitOps-driven**, built on **plain Docker** with static trust-zoned placement across three nodes, organized around **projects** — each project is its own GitHub organization, fully managed by the platform.

Three custom Rust services form the control plane:

- **GitHub Bot** (`crates/bot`) — the liaison. The only component talking to the GitHub and Tailscale APIs: org reconciliation, digest bumps, manifest rendering (render PRs onto `env/<class>` branches), membership + ACL sync, repo proxy for the reconciler, dashboard write API.
- **Reconciler** (`crates/reconciler`) — the orchestrator. Consumes rendered `env/*` branches, decrypts SOPS secrets with class keys, and converges each node's Docker API over WireGuard: blue-green deploys, per-project networks/ingress/DB provisioning, ephemeral GC.
- **Setup** (`crates/setup`) — the provisioner (ADR 0004). First-run wizard (GitHub App via manifest flow, platform repo seeding) + node enrollment over SSH.

**Credential isolation:** the bot holds the GitHub App key + Tailscale API key; the reconciler holds age keys + Docker mTLS certs; setup holds the enrollment SSH key + PKI CA. Disjoint powers.

📄 **Full design:** [docs/design.md](docs/design.md) · **Roadmap:** [docs/roadmap.md](docs/roadmap.md) · **Diagrams:** [docs/diagrams/](docs/diagrams/)

> This repo is the platform **source code only**. Live platform config lives in GitHub: the `majksa-platform/platform` repo (nodes, people, project registry — seeded from [`platform-seed/`](platform-seed/)) and one `ops` repo per project org.

## Quick start

### Installing the platform (operators)

Create the root GitHub org by hand (the one manual step, §2), then on a fresh Debian machine — the future **main** node:

```sh
curl -fsSL https://raw.githubusercontent.com/maxa-ondrej/majnet/main/bootstrap/install.sh | bash
```

The installer bootstraps the node, generates all key material, starts the control plane, and prints a **setup-wizard URL**: create the GitHub App there (manifest flow — one click), seed the platform repo, and enroll the prod/private nodes by handing the wizard SSH access. See [ADR 0004](docs/adr/0004-setup-service-auto-provisioning.md) and [`crates/setup/README.md`](crates/setup/README.md).

Break-glass / manual path: the [`bootstrap/`](bootstrap/README.md) scripts remain runnable standalone, and the crate READMEs document every env var. Day-2 operations live in [`docs/runbooks/`](docs/runbooks/).

### Hacking on the platform

Everything you need comes from **nix + direnv** ([hook direnv into your shell](https://direnv.net/docs/hook.html) first):

```sh
git clone git@github.com:maxa-ondrej/majnet.git && cd majnet
direnv allow          # builds the dev shell: Rust, clippy, rust-analyzer, sops, age, plantuml
cargo test --workspace && cargo clippy --workspace
```

Then prove the core actually works — the smoke test runs the reconciler's full loop (converge → SOPS secret on tmpfs → blue-green → GC) against your **local Docker daemon**, no servers or GitHub needed:

```sh
scripts/smoke-test.sh
```

## Topology

| Node | Trust zone | Runs |
|---|---|---|
| **main** | control plane | bot, reconciler + DB, dashboard, Dozzle, Beszel |
| **prod** | public workloads | `edge-main` (Traefik), production apps + DBs |
| **private** | internal workloads | per-project ingresses, stable + ephemeral apps, dev DBs |

Environment classes: `production` (public, gated by a reviewed render PR), `stable` (VPN, auto-deploy), `ephemeral` (VPN, PR-scoped, TTL'd).

## Repository layout

```
majnet/
├── Cargo.toml            # Rust workspace
├── crates/
│   ├── common/           # shared types: manifest schema, project + platform config
│   ├── bot/              # GitHub Bot (liaison)
│   ├── reconciler/       # Reconciler (orchestrator)
│   └── setup/            # Setup (provisioner: wizard + node enrollment)
├── dashboard/            # web UI over reconciler (reads) + bot (writes)
├── bootstrap/            # node bootstrap: Docker, WireGuard, roles, firewall (bash, Debian)
├── platform-seed/        # initial content for the majksa-platform/platform repo
├── templates/
│   └── repo-templates/   # app repo templates (GHA workflow, branch protection)
└── docs/
    ├── design.md         # the design document (source of truth)
    ├── roadmap.md        # phased roadmap + status
    ├── adr/              # architecture decision records
    ├── diagrams/         # PlantUML + Mermaid sources
    └── runbooks/         # operational runbooks
```

Note: this monorepo holds the **platform source code**. Live platform *config* lives in GitHub — the root `majksa-platform/platform` repo (nodes, people, project registry) and each project org's `ops` repo. See design doc §2 and §10.

## Development

The toolchain is provided by **nix + direnv** (`flake.nix` + `.envrc`): Rust (rustc, cargo, clippy, rustfmt, rust-analyzer), plus `sops`, `age`, and `plantuml`. With [direnv hooked into your shell](https://direnv.net/docs/hook.html), `cd` into the repo and run `direnv allow` once — the environment loads automatically from then on (cached via nix-direnv).

```sh
cargo build            # build the workspace
cargo test             # run tests
cargo run -p majnet-bot
cargo run -p majnet-reconciler
```

## Status

All roadmap phases (0–6) are **code-complete with CI**; what remains is live wiring against real servers and a real GitHub org. See [docs/roadmap.md](docs/roadmap.md) for the per-phase checklists.
