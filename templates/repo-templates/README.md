# App repo templates

Templates the bot uses to materialize app repos declared in a project's `project.yaml` (e.g. `template: rust-service`, `template: web-app`). Each template ships two GHA workflows for the DEV→OPS gradient (ADR 0009):

- **`build.yaml`** — the *build tier*: on `main`/PR, test → build → push image to GHCR by digest. Feeds `testing` (latest main) and `ephemeral` (PR previews). Disposable, continuous.
- **`release.yaml`** — the *release tier*: on tag `vX.Y.Z`, calls the reusable [`app-release.yaml`](../../.github/workflows/app-release.yaml) — builds + pushes `image:vX.Y.Z` to GHCR by digest. That tagged publish *is* the release: the bot records it off the `registry_package` webhook (version→digest) and auto-tracks `stable`; an operator promotes a chosen version to `production`. The migration lives in the ops overlay, not here.

Plus branch protection config for `main` and standard labels.

Each template also ships a **minimal runnable scaffold** — a Dockerfile and a tiny server — so a freshly materialized app is deployable and already serves the platform **standard endpoints** (design doc §16):

- **`/healthz`** — liveness. This is the platform's default `health.path`, so an app's manifest need only declare the `health.port`.
- **`/info`** — build metadata as JSON: `{version, commit, build_time}`. The reconciler scrapes this right after the health gate and shows it per env in the dashboard.

Build metadata flows from CI into the image: `build.yaml`/`release.yaml` pass `VERSION`/`GIT_COMMIT`/`BUILD_TIME` as Docker **build-args**; the Dockerfile bakes them into `ENV`; the server reads them at `/info`. The build tier reports `version: "dev"`; the release tier stamps the `vX.Y.Z` tag. Replace the scaffold's catch-all handler with your real app — keep `/healthz` + `/info`.

```
rust-service/   Cargo.toml · src/main.rs (axum) · Dockerfile · workflows
web-app/        package.json · server.js (node:http) · Dockerfile · workflows
```

These are *developed* here and *deployed* to `majksa-platform/platform/repo-templates/`, which is what the bot actually reads (design doc §10).

## Monorepo apps (ADR 0018)

A monorepo — one GitHub repo hosting several apps — is **bring-your-own CI**: the platform doesn't scaffold or archive it, so it ships no `build.yaml`. Instead the repo owner wires the reusable build-tier workflow [`app-build.yaml`](../../.github/workflows/app-build.yaml), which builds + pushes each app's **nested** image `ghcr.io/<org>/<repo>/<app>` with the same build-tier tags a solo `build.yaml` produces (`pr-<N>` → preview, `sha-…`/`latest` → testing). The `registry_package` webhook maps the package's last segment to the app, so no wiring beyond publishing the image is needed. The `vX.Y.Z` release tier is the same reusable [`app-release.yaml`](../../.github/workflows/app-release.yaml) solo apps use.

```yaml
# .github/workflows/build.yaml in the monorepo — one app per matrix entry:
on: { push: { branches: [main] }, pull_request: }
jobs:
  build:
    strategy:
      matrix:
        app:
          - { name: api, context: apps/api }
          - { name: web, context: apps/web }
    permissions: { contents: read, packages: write }
    uses: majnet/majnet/.github/workflows/app-build.yaml@main
    with:
      app: ${{ matrix.app.name }}
      context: ${{ matrix.app.context }}
```

Gate each app on its own `paths:` (or a paths-filter step) so a PR touching one app doesn't rebuild the whole repo; run tests in the caller — `app-build.yaml` only builds + publishes.
