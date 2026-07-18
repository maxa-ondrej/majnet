# 0018 — Monorepo apps (one repo, many apps)

**Status:** accepted (phase 2) · **Date:** 2026-07-19 · relates to [0009](0009-releases-are-tagged-image-publishes.md), [0012](0012-private-image-pull-auth.md)

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

**Phase 2** makes the remaining bot-side repo operations repo-aware (they no
longer assume `app == repo`):

- **Cut-release is repo-wide.** A monorepo tag is one shared `vX.Y.Z` version
  line: `cut` resolves the app's `repo`, tags `/repos/<org>/<repo>` at `main`
  HEAD, and computes both the "last version" and (for `bump=auto`) the
  commit range over the whole repo — the max release across every app sharing
  it. One tag releases every app in the monorepo. (`releases.rs::app_repo`,
  `do_cut`, `commits_since`, `resolve_commit`.)
- **PR-preview comments** post to the app's actual repo (the monorepo) and carry
  a **per-app marker** (`<!-- majnet-preview:<app> -->`), so several apps'
  previews on one shared PR each get their own comment instead of clobbering a
  single one. (`ephemeral.rs::comment_preview_url`.)
- **Provenance** now resolves against the repo, so monorepo releases record the
  tag's commit SHA like solo apps.
- **Dashboard.** New-app creation exposes an optional "Monorepo repo" field
  (`NewApp.repo`), so a monorepo app can be declared from the UI, not only via
  `project.yaml` / API.

## Consequences

- Monorepo apps deploy, release, and preview end to end: declare with `repo`
  (UI or `project.yaml`), push nested images, cut repo-wide releases, and get
  per-app PR previews. The shared repo is left untouched (never scaffolded or
  archived).
- **Rename is rejected for monorepo apps.** Renaming a monorepo member can't
  rename its shared repo (that repo hosts siblings), so `rename` returns a clear
  error pointing the user at `project.yaml`. Full monorepo rename (repo
  untouched, nested GHCR package `<repo>/<old>`→`<repo>/<new>` copied, nested pin
  rewritten) is **phase 3**.
- **Reusable build CI for BYO monorepos.** The build tier is a reusable
  workflow, `.github/workflows/app-build.yaml`: a monorepo owner calls it once
  per app (matrix), and it builds + pushes that app's nested image with the same
  build-tier tags a solo `build.yaml` produces (`pr-<N>` → preview,
  `sha-…`/`latest` → testing). No bot change is needed — the existing
  `registry_package` → leaf-app mapping handles nested packages. The `vX.Y.Z`
  release tier reuses `app-release.yaml`. Remaining phase-3 nicety: scaffolding
  this caller automatically (today the owner adds it — it's bring-your-own CI).
- App names remain unique within a project (already true) — required for the
  package-leaf → app mapping.
