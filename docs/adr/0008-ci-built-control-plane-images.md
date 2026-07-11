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

1. Add swap (done) — belt-and-braces for the transitional native builds.
2. `docker login ghcr.io` on main with a token that can pull the packages (or
   make the two packages public — simplest for a single-tenant control plane).
3. Confirm the bot's PEM path in `/etc/majnet/bot.env`
   (`MAJNET_GITHUB_PRIVATE_KEY_PATH`) and align the bot mount in
   `deploy/compose.yaml`.
4. `systemctl disable --now majnet-bot majnet-reconciler` (stop the native
   services) and the dashboard's old compose.
5. `docker compose -f /opt/majnet/deploy/compose.yaml up -d` with the image env
   vars set to the pinned digests.
6. Flip `majnet-update` to v2 and repoint `version.yaml` at the digests.

## Rollback

The native binaries and systemd units remain on disk; `systemctl enable --now
majnet-bot majnet-reconciler` restores the pre-cutover state. Keep both paths
working until a couple of clean pull-based updates have landed.

## Open items

- **GHCR pull auth on the node** (the pre-existing "private GHCR" gap) — resolve
  by `docker login` with a read:packages token, or publish the two control-plane
  packages publicly (they contain no secrets).
- **setup binary** — publish from CI as a release asset so *nothing* compiles on
  the node; until then setup is still built by the installer.
- **Multi-arch** — CI builds `amd64` only (the fleet is amd64).
