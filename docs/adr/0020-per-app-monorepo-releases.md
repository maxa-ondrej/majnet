# 0020 — Per-app monorepo releases + autorelease

**Status:** accepted · **Date:** 2026-07-20 · relates to [0018](0018-monorepo-apps.md), [0009](0009-dev-to-ops-delivery.md)

## Context

ADR 0018 made a monorepo's release **repo-wide**: `cut` tags
`/repos/<org>/<repo>` at `main` HEAD with a plain `vX.Y.Z`, and that one tag
releases every app in the repo (last-version + `bump=auto` range computed across
all its apps). That is a poor fit for a repo migrating from **per-package
versioning** (Changesets-style, tags `@<scope>/<app>@X.Y.Z`, one GitHub Release
per app per version), where the owner wants to keep per-app tag naming and an
independent cadence per app.

The reference user is `sideline-cz/sideline` (5 apps: proxy, server, web, docs,
bot), whose 446 historical releases are `@sideline/<app>@<ver>`. It could not use
the reusable `app-release.yaml` (which derives the image tag from
`github.ref_name` — invalid for a prefixed tag) and had to hand-roll a workflow
that parses `@sideline/<app>@<ver>` → (app, version) and pushes
`ghcr.io/<org>/<repo>/<app>:<ver>`. That works because the bot only reads the
**image** tag off the `registry_package` webhook, but it degrades: every monorepo
re-hand-rolls the same CI, `cut` still emits inconsistent repo-wide tags, and
provenance can miss the scoped git tag.

The reconciler/ops/render layers are release-agnostic (they key on `apps/<app>/`
+ the pinned digest), and the release **store is already per-app**
(`(org, app, version)`). The only repo-wide machinery is cut/draft.

## Decision

Add an optional **`release:` block on `AppDecl`** (GitOps — in `{org}/ops`
`project.yaml`, written by the dashboard via a plain commit to ops `main`, never
a PR):

```yaml
apps:
  - name: sideline-server
    template: byo
    repo: sideline
    release:
      scope: sideline            # per-app tags @sideline/<leaf>@vX.Y.Z
      autorelease: auto          # off | patch | auto   (phase 2)
      paths: [applications/server/**]
```

- **Per-app is opt-in via `release.scope`.** Scope present ⇒ the app releases
  with a **scoped git tag** `@<scope>/<leaf>@<version>` (leaf = `image_leaf`, the
  app name minus the `<repo>-` prefix), each app on its own version line. Scope
  absent ⇒ today's repo-wide `vX.Y.Z` (unchanged, backward compatible). The
  version prefix (`v` / bare) is preserved from the app's existing releases, as
  before (`releases::LastRelease`).
- **The release *mechanism* is unchanged: git-tag → CI rebuild.** `cut` (and a
  draft `submit`) create the tag as a git ref (not a PR); the repo's release
  workflow rebuilds the version-stamped image and pushes it; the
  `registry_package` webhook records the release per-app. The **image** tag is
  still the plain version (`:v0.39.1`) on the nested package `<repo>/<leaf>`, so
  `digest.rs::on_package_published` maps it to `<repo>-<leaf>` and records it with
  no change — only the *git* tag is scoped.
- **Release unit.** A "release unit" is the app in per-app mode, else the shared
  repo. `AppDecl::release_unit` keys drafts and the "last version"; a repo can
  host several units (each per-app app + one repo-wide unit for the rest).
  `on_app_main_push` refreshes one draft per unit.
- **Cut / draft are per-unit.** Per-app: last-version + commit range are this
  app's only, and the tag is the scoped tag. A bulk `POST …/{repo}/cut-repo`
  cuts every app in a monorepo in one action (each at its own next version).
- **Per-app changelog is path-scoped.** For a per-app unit with `paths`, the
  changelog + `auto`-bump diff lists only commits that touched those paths (the
  commits API filtered by `path`, bounded by the base commit's date) — so a busy
  monorepo doesn't inflate one app's changelog with sibling changes. No `paths`
  (or a leading-glob pattern) falls back to the whole-repo `base...main` diff.
- **A release pushes the version bump + changelog into the repo.** For a per-app
  unit with a derivable app dir (the literal prefix of its first `release.paths`
  glob, e.g. `applications/server`), cut/submit first commit — directly to `main`,
  one commit, no PR — the bumped `<dir>/package.json` `"version"` (bare semver,
  formatting/key-order preserved via a targeted edit, not a serde round-trip) and
  a prepended `<dir>/CHANGELOG.md` entry, **then tag that commit**. The commit
  message carries a `chore(release): ` marker, and `on_app_main_push` skips
  autorelease for such a push — so a release never re-triggers a release. The
  fast-forward ref update means a concurrent push errors rather than being
  clobbered. No `paths` / no package.json → the file push is skipped (the release
  still tags).
- **Provenance** resolves the app's *configured* scoped git tag first
  (`resolve_commit` → `AppDecl::release_tag`), covering a scope that differs from
  the repo name; the legacy `@<repo>/<leaf>@<ver>` and plain `vX.Y.Z` remain
  fallbacks.
- **Bump mapping is configurable, ignore-by-default.** `auto` bumps + the
  changelog derive from a commit-type → bump map: a breaking change
  (`type!` / `BREAKING CHANGE`) is always **major**; the default maps
  `feat` → minor, `fix` → patch and **ignores every other type** (chore/docs/
  refactor/… no longer force a patch or clutter the changelog). Override per app
  with `release.bumps` (`{ <type>: major|minor|patch }`, e.g. add `perf: patch`).
  A commit range with no releasable commit is **no candidate**: an `auto` cut
  errors "nothing to release" and the draft clears. (`classify_bump` /
  `generate_changelog` take the rules; `AppDecl::bump_rules`, `default_bump_rules`.)
- **Reusable CI.** `app-release.yaml` gains `leaf` + `version` inputs (image
  nests at `ghcr.io/<owner>/<repo>/<leaf>:<version>`, `VERSION` baked for
  `/info`); unset ⇒ `github.ref_name` (solo apps unchanged). Template-sync seeds
  a per-app **release caller** (`.github/workflows/release.yaml`) into a per-app
  monorepo that lacks one — a resolve job parses `@<scope>/<leaf>@<ver>` and maps
  the leaf → build context, then calls `app-release.yaml`. A one-time
  `monorepo-release-ci` PR (setup, distinct from the release action; never
  overwritten).

**Autorelease** (`autorelease: patch|auto`, `paths` globs): on a push to `main`,
each release unit whose `paths` match a changed file (gitignore-style globs via
`globset`; changed files come from the push payload) is auto-cut — `patch` always
bumps patch, `auto` derives the bump from conventional commits (reusing the same
`do_cut` tag→CI path). Opt-in per app; manual `cut` still coexists; autorelease
units skip the draft (they auto-cut instead). An `auto` unit with no unreleased
commits is a benign no-op. (`releases::on_app_main_push` → `try_autorelease` /
`paths_match`; `webhooks::changed_paths`.)

## Consequences

- A monorepo keeps **per-app tag naming + cadence** first-class: cut/draft/
  provenance/CI all understand `@<scope>/<leaf>@<ver>`, and MajNet seeds the CI
  so no repo hand-rolls the tag-parsing workflow. `sideline` can drop its custom
  `resolve` job once its apps set `release.scope: sideline`.
- **Backward compatible.** No `release` block ⇒ repo-wide `vX.Y.Z` verbatim
  (record + auto-track every app off a plain tag; `app-release.yaml` via
  `github.ref_name`). The draft store is unchanged — the same table, keyed by the
  release unit (a text key).
- Release **policy lives in git** (`project.yaml`) — dashboard-configured, no
  hand-editing, no PR — so it is reviewable and travels with the ops repo, at the
  cost of one more field on `AppDecl`.
- Amends ADR 0018 ("cut-release is repo-wide") and ADR 0009 ("a release is a
  `vX.Y.Z`-tagged image publish") for the per-app case: the tag is scoped, the
  image tag stays the bare version.
