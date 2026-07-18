# 0018 — Monorepo apps (one repo, many apps)

**Status:** accepted (phase 1) · **Date:** 2026-07-19 · relates to [0009](0009-releases-are-tagged-image-publishes.md), [0012](0012-private-image-pull-auth.md)

## Context

MajNet fused one identity: an app's `name` was simultaneously its GitHub repo,
its GHCR package, its ops directory `apps/<name>/`, its manifest `name`, and its
runtime container/volume/DB name. That made "one repo = one app" a hard
assumption across the bot (repo scaffold/archive, the `registry_package`
webhook → app mapping, image naming, releases, rename, delete).

Users want **one GitHub repository to host several MajNet apps** (a monorepo).
The reconciler and the ops/render layers were already repo-agnostic — they key
on `apps/<app>/` and the manifest `name` — so the coupling to break is entirely
bot-side and image-side.

## Decision

Add an optional `repo` to `AppDecl` in `project.yaml`. Apps that share a `repo`
value are one monorepo; an app with no `repo` keeps its own repo named after it
(fully backward compatible).

- **Image naming.** A monorepo app's image nests: `ghcr.io/<org>/<repo>/<app>`
  (matching the control-plane multi-segment convention). A solo app is unchanged
  at `ghcr.io/<org>/<app>`. (`AppDecl::image_base`.)
- **Webhook → app.** A GHCR package name is `<app>` (solo) or `<repo>/<app>`
  (nested). The MajNet app is always the **last segment** — it keys `apps/<app>/`,
  the manifest name, and the runtime name; the full nested path is preserved in
  the pinned image. App names are unique within a project, so the leaf is
  unambiguous. (`digest.rs::on_package_published`.)
- **Ops layout unchanged.** Config still lives at `apps/<app>/` (flat, per
  project). Render/promote/track-stable/`record` need no change.
- **Repo lifecycle is per-repo, bring-your-own.** A monorepo repo is **not**
  scaffolded from a template and **never archived** by org-sync while any app
  references it; the platform only consumes the images it publishes. Solo app
  repos are scaffolded/archived as before. (`org_sync.rs`.)

**CI is the repo owner's** (phase 1): the monorepo's own workflow builds and
pushes the per-app images (`vX.Y.Z` / `sha-…` / `pr-N` tags on
`ghcr.io/<org>/<repo>/<app>`). MajNet consumes them via the webhook.

## Consequences

- Monorepo apps deploy end to end today: declare with `repo`, push nested
  images, MajNet renders + deploys them, and the shared repo is left untouched.
- **Phase 2 (not yet):** the per-app repo operations still assume `app == repo`
  and are unsupported for monorepo apps — **cut-release** (tags `/repos/<org>/<app>`),
  **rename**, and the **PR-preview comment** (posts to the app's own repo). A
  repo-wide tag/PR in a monorepo maps to several apps; that fan-out + a scaffolded
  matrix CI are the phase-2 work. `resolve_commit` provenance is best-effort and
  simply returns empty for monorepo releases.
- App names remain unique within a project (already true) — required for the
  package-leaf → app mapping.
