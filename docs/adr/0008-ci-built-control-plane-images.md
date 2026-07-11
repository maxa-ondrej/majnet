# ADR 0008 — CI-built control-plane images (amends ADR 0005)

**Status:** accepted (implementation in progress)
**Date:** 2026-07-10

## Context

ADR 0005 converges the control plane by pinning a git ref in
`platform/version.yaml` and having `majnet-update` **compile it on the main
node** (`cargo build --release`). On a 1.9 GB VPS this is the wrong place to
build:

- The release build of the bot (axum + octocrab + tokio) **OOM-kills** with no
  swap.
- `majnet-update` does `git checkout <ref>` **before** building, so a failed
  build strands the node: HEAD is already at the target, every later run says
  "already at" and skips → the binary never updates (observed live: the box ran
  a stale bot for two days).
- It contradicts the platform's own principle — everything else is **pinned by
  digest, never built from a tag**. The control plane should eat its own dog
  food.

## Decision

Build the control plane in CI and ship **digest-pinned images** from GHCR; the
node **pulls** instead of compiling.

- `.github/workflows/images.yaml` builds two images on every push to `main`
  (and tags), pushing to `ghcr.io/maxa-ondrej/majnet/{control-plane,dashboard}`
  tagged `sha-<full>` + `latest`:
  - **control-plane** — `majnet-bot` + `majnet-reconciler` binaries + the tools
    they shell out to (`age`, `openssl`, `sops`). One image, two containers.
  - **dashboard** — the Vite/React build served by nginx.
- `setup` is **not** containerized: it drives `systemctl`/`wireguard`/`git` on
  the host. It stays a native binary (later: a CI-published release asset).

### Credential isolation by mounts

The design's §6 invariant (bot = GitHub + Tailscale + Cloudflare; reconciler =
age keys + Docker mTLS certs) is preserved by **what each container mounts**,
not by separate images (`deploy/compose.yaml`):

- **bot** mounts only its GitHub App PEM + `/var/lib/majnet-bot`. Its other
  credentials (Cloudflare token, Tailscale key, webhook secret, age *public*
  recipient) are env strings in `bot.env`.
- **reconciler** mounts only `/etc/majnet/age` + `/etc/majnet/pki` + its data
  dir.

Neither can read the other's secrets. Both use `network_mode: host` because
they bind the WireGuard IP (`10.88.0.1`).

### version.yaml (new schema)

```yaml
control_plane:
  # Digest-pinned CI images (ADR 0008).
  image: ghcr.io/maxa-ondrej/majnet/control-plane@sha256:…
  dashboard: ghcr.io/maxa-ondrej/majnet/dashboard@sha256:…
  # Git ref for the setup binary + bootstrap payload (still ref-based).
  ref: <git sha>
```

`majnet_common::platform::VersionFile` gains `image` + `dashboard`; `ref`
stays (for setup/bootstrap). `GET /api/platform/version` keeps returning `ref`.

### majnet-update v2

Reads the pinned image digests, then:

```sh
docker login ghcr.io -u <bot> --password-stdin      # GHCR pull auth
export MAJNET_CONTROL_PLANE_IMAGE=<image> MAJNET_DASHBOARD_IMAGE=<dashboard>
docker compose -f /opt/majnet/deploy/compose.yaml pull
docker compose -f /opt/majnet/deploy/compose.yaml up -d
# setup binary: git fetch <ref> + download/install the CI release asset
```

No `cargo`, no on-box build, no checkout-before-build footgun. Idempotent: an
unchanged digest is a no-op pull.

## Apply (one-time cutover on main)

The **first** bring-up is manual, because the running (old) bot serves the
ref-only `version.yaml` endpoint — v2 `majnet-update` can't read the image pins
from it until the new bot is up. After this, the v2 timer maintains everything.

Prereqs (done): swap added; GHCR packages made **public** (no `docker login`);
PEM confirmed at `/etc/majnet/github-app.pem` (matches the compose mount).

```sh
NEWREF=<sha>
IMG=ghcr.io/maxa-ondrej/majnet/control-plane@sha256:…
DASH=ghcr.io/maxa-ondrej/majnet/dashboard@sha256:…

# 1. Bring /opt/majnet to the new ref (compose file + v2 majnet-update).
sudo git -C /opt/majnet fetch --depth 1 origin "$NEWREF" && sudo git -C /opt/majnet checkout "$NEWREF"
sudo install -m0755 /opt/majnet/bootstrap/majnet-update /usr/local/bin/

# 2. Retire the native services + the standalone dashboard.
sudo systemctl disable --now majnet-bot majnet-reconciler
sudo docker compose -f /opt/majnet/dashboard/compose.yaml down

# 3. Bring up the control plane from the pinned images.
sudo MAJNET_CONTROL_PLANE_IMAGE="$IMG" MAJNET_DASHBOARD_IMAGE="$DASH" \
  docker compose -f /opt/majnet/deploy/compose.yaml up -d

# 4. Hand off to the v2 timer: pin version.yaml to the ADR 0008 schema
#    (ref + image + dashboard). From here, `majnet-update` just pulls.
```

## Rollback

The native binaries and systemd units remain on disk; `systemctl enable --now
majnet-bot majnet-reconciler` + `docker compose -f deploy/compose.yaml down`
restores the pre-cutover state. Keep both paths working until a couple of clean
pull-based updates have landed.

## Resolved follow-ups

- **GHCR pull auth** — the two packages are **public** (set in the web UI; the
  REST API / gh CLI have no visibility endpoint). No `docker login` on the node.
- **setup binary** — `setup` now rides in the control-plane image and
  `majnet-update` extracts it (`docker cp`); **nothing compiles on the node.**
- **Fresh installs** — `install.sh` drops the Rust/build toolchain, pulls the
  images, extracts setup, and runs the control plane as compose. The wizard's
  `restart_bot` uses `docker compose up -d bot` (the bot's `env_file` is optional
  so compose parses before the wizard writes creds). `70-dashboard.sh` uses
  `deploy/compose.yaml`.
- **Cleanup** — `bootstrap/majnet-cleanup` removes the dead Rust toolchain, C
  build deps, cargo cache, and legacy native bot/reconciler units from a node
  installed the old way (one-shot, idempotent).

## Still open

- **Multi-arch** — CI builds `amd64` only (the fleet is amd64).
- **Fresh-install flow is untested on a real VPS** — only the live upgrade path
  (via `majnet-update`) is verified. Validate on the next real install.
