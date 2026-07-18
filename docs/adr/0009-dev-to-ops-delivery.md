# ADR 0009 — DEV→OPS delivery: builds, releases, and the class gradient

**Status:** accepted (design; implementation phased)
**Date:** 2026-07-12

> **Revision 2026-07-12 — the release descriptor is gone.** The original design
> shipped a `majnet-release.yaml` *descriptor* as a GitHub Release asset. In
> review we found it re-transmitted, over a flaky channel (asset upload →
> `release` webhook → download → backfill), information the bot **already
> receives**: the digest + version arrive on the `registry_package` webhook that
> already drives the build-tier bumps, and the migration is **already an ops
> overlay field**. So: **a release is now just a `vX.Y.Z`-tagged image
> publish.** No descriptor, no `release` webhook, no asset backfill. The
> migration is configured in the ops `base.yaml` (a version-independent command;
> the migration *files* travel in the image). Sections below are updated; struck
> mechanics are called out inline.

## Context

OPS is fully GitOps'd — the `ops` repo pins images by digest, the bot renders
`env/*`, the reconciler converges — but the **DEV→OPS handoff is a manual
`image: …@sha256:` edit**. There is no first-class path from an app's source
repository to its running environments.

We already have most of the machinery: multi-org webhook intake, digest bumps
(§11.4: stable auto-on-merge, production via promote), the render → `env/*` →
reconciler loop, the manifest's `migration` + `database` fields, GHCR-by-digest,
and the GitHub App's subscription to `push` / `pull_request` /
`registry_package` events. This ADR wires those into a delivery pipeline.

## Decision

### Two tiers: **builds** vs **releases**

- **Builds** (PR, main) — *an image digest only*. Disposable, continuous, no
  ceremony. They feed the throwaway zones.
- **Releases** (git tags `vX.Y.Z`) — a **`vX.Y.Z`-tagged image publish**,
  recorded by the bot as an immutable version→digest pin. This is **the DEV→OPS
  contract**: versioned, shown on the dashboard, and promotable.

### The class gradient (adds `testing`)

```
PR opened ──────▶ ephemeral   per-PR preview, TTL-GC'd        (build)
merge to main ──▶ testing     continuous, latest main         (build)   ← NEW class
tag vX.Y.Z ─────▶ stable      versioned release, auto          (release)
promote ────────▶ production  a chosen release, admin-gated    (release)
```

- `EnvClass` gains **`Testing`**. Static placement holds:
  `testing`/`stable`/`ephemeral` → **private** node, `production` → **prod**.
- **`stable` is re-pointed from merge-driven to tag-driven** (amends §11.4:
  "merge → stable" becomes "merge → testing, tag → stable"). This makes
  `stable` genuinely stable — versioned — instead of "whatever's on main."
- Per-app overlays become `base` + `testing`/`stable`/`production`/`ephemeral`.

### A release = a `vX.Y.Z`-tagged image publish

On a tag `vX.Y.Z`, the app's CI builds + pushes `ghcr.io/<org>/<app>:vX.Y.Z` by
digest. That publish fires the **`registry_package` webhook** (the same event
that drives the testing/ephemeral bumps), carrying both the tag and the digest.
The bot reads the tag:

- `pr-<N>` → ephemeral preview;
- `vX.Y.Z` → **record a release** `(version, digest, commit)` and auto-track it
  into `stable`;
- anything else (`latest`, `sha-…`) → `testing`.

Commit provenance is resolved best-effort from the tag via the commits API.
There is **no descriptor file and no `release` webhook** — the digest is on the
webhook, and the release record *is* the version→digest pin.

### Migrations live in the ops overlay (`migration = { image?, command }`)

The manifest's `migration` (an optional `image` + a `command`) is configured in
the ops `base.yaml`, next to the DB/secret config it depends on — **not** shipped
per-release. This works because the migration *command* is version-independent
(`["rails","db:migrate"]`, `["dbmate","up"]`) while the migration **files travel
inside the app image**: `promote vX.Y.Z` pins that image, and the reconciler's
§12 step runs the command against it, applying that version's migrations.

- **App-image migration** — omit `image`; `command` runs in the promoted app
  image. This is the common case.
- **Separate migration image** — set `migration.image` to a digest-pinned runner
  (operator-pinned in the overlay, since a separate image's digest has no
  auto-delivery path).

### App CI (reusable workflow, shipped in a starter template)

- MajNet owns a **reusable GitHub Actions workflow** (`build → push a
  version-tagged image by digest`), just like the control-plane `images.yaml`.
  Continuous main/PR builds go through the app's `build.yaml`; `release.yaml`
  handles `v*` tags.
- A **starter template repo** (`templates/repo-templates/`) wires both workflows
  in, so a new app starts delivery-ready; the pipeline updates centrally through
  the reusable workflow rather than drifting per-app.

## What this reuses vs adds

| Reused | Added |
|---|---|
| Webhook intake, render PRs, digest bumps, `env/*`→reconciler, the §12 migration step, GHCR-by-digest, the §9 production gate | `EnvClass::Testing`, `migration.image`, a bot **release store** (version→digest, keyed off `registry_package`), build-tier image bumps (PR/main → ephemeral/testing), a dashboard **Releases** tab, the reusable workflow + starter template |

## Component changes

- **common** — `EnvClass::Testing` (+ `node_role`, `ALL`, `as_str`,
  `env_branch`); `Migration.image: Option<String>`.
- **bot** — on `registry_package`, a `vX.Y.Z` tag → record the release
  `(version, digest, commit)` and auto-track `stable`; `pr-<N>` → ephemeral;
  else → `testing`. `promote` writes a chosen release's app digest into
  `production.yaml` (migration inherited from `base.yaml`; existing digest-bump
  + render-PR path).
- **reconciler** — run `migration.image` (default app image) with `command`,
  from the rendered manifest (unchanged).
- **dashboard** — per-app **Releases** tab: versions, artifact digest, on-prod
  marker, "Promote → production"; testing/ephemeral show the current build.
- **templates/repo-templates** — the starter app repo + `build.yaml`/`release.yaml`.

## Phasing

1. ✅ **`EnvClass::Testing`** — schema + render + converge + dashboard.
2. ✅ **Bot release store** — SQLite `releases` table (version→digest),
   `GET /api/releases/{org}/{app}`. *(Rev 2: fed by `registry_package`, not a
   `release` webhook + descriptor.)*
3. ✅ **Dashboard Releases tab + promote-from-release** — `migration.image`,
   `POST …/releases/…/promote/{version}`, per-app Releases panel.
4. ✅ **Reusable workflow + templates** — `.github/workflows/app-release.yaml`
   (build → push a `vX.Y.Z`-tagged image by digest); `release.yaml` added to the
   web-app + rust-service templates. *(Rev 2: no descriptor asset / GitHub
   Release; the tagged publish is the release.)*
5. ✅ **Build-tier wiring** — a `main` build bumps `apps/<app>/testing.yaml`
   (was `stable.yaml`); a `vX.Y.Z` publish records a release and re-points
   `apps/<app>/stable.yaml` at the newest tag. Both are **opt-in by
   overlay-presence** (matching `render`): an absent overlay skips the bump,
   never creates it. `pr-<N>` builds still feed `ephemeral`.

## Open items

- ✅ **Draft releases (review-gated cuts)** — rather than cut on every push, the
  bot prepares a **draft**: the proposed next version (semver from the last
  release) + a generated changelog (conventional commits grouped into
  Breaking/Features/Fixes/Other), refreshed on each push to the app repo's
  `main` and stored per repo (repo-wide for a monorepo). The dashboard Releases
  panel shows it with editable notes; **submitting** (`POST …/draft/submit`,
  admin) tags the repo at `main` HEAD and runs the same cut→CI→record flow, and
  the changelog is persisted per release (`release_notes`, shown under each
  release). Nothing auto-releases — a draft waits for an operator. Endpoints:
  `GET`/`DELETE …/draft`, `POST …/draft/refresh`, `PUT …/draft/notes`,
  `POST …/draft/submit`. Operator-edited notes survive a push refresh.
- ✅ **Release backfill** — a missed `registry_package` for a `vX.Y.Z` tag left
  the store (and stable) unaware of that release with no self-heal. Recovered
  on demand: `releases::backfill` enumerates **GHCR package versions**
  (tag→digest is authoritative there) and records any missing version-tagged
  one; exposed as `POST /api/releases/{org}/{app}/backfill` (Developer-gated)
  with a "Backfill from registry" button on the app's Releases panel. Needs
  `packages:read` on the installation token (already requested).
- Production promote: allow any release, or only newer-than-current?
- `ephemeral` still builds per-PR; confirm it stays digest-from-PR-build (yes).
