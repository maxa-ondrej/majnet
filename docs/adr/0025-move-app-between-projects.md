# 0025 — Move an app/service between projects

**Status:** proposed (pre-implementation) · **Date:** 2026-07-23 · relates to [0009](0009-shared-versioning.md), [0010](0010-app-migration-from-external-paas.md), [0018](0018-monorepo-support.md), [0019](0019-network-aliases.md); builds on the app/project **rename** feature

## Context

The app lifecycle today is create / rename / archive / delete **within one project**
(= one GitHub org). There is no way to relocate an app to a *different* project.
Because a project maps 1:1 to a GitHub org and **prefixes every runtime resource** —
container `<project>-<app>-<class>-…`, volume `majnet-<project>-<app>-<class>-<vol>`,
DB `<project>_<app>_<class>`, the `proj-<project>` network, ingress hosts under the
project domain — a move is a **cross-org rename of the data prefix**, not a config
edit.

Prerequisite already satisfied: the reconciler's data-cutover primitive
`rename::migrate_stack` is written on **both axes** — `(old_project, old_app) →
(new_project, new_app)` with the project and app varying independently. App-rename
holds the project fixed; a **move holds the app fixed and varies the project**. So
the reconciler side needs almost no new code — only a new bot orchestration + a
freeze keyed on the destination project.

Inline secrets (ADR 0024) are encrypted to the **platform class recipient**, not a
per-project key, so a move does **not** re-encrypt secrets — they travel verbatim in
the manifest. (This retires the pre-0024 "re-encrypt to the destination admins"
worry.)

## Decision

Add a **`POST /api/apps/{org}/{app}/move`** bot endpoint (admin of **both** the
source and destination projects) that performs a cross-org rename of the data
prefix, modeled on `app_rename_post` but spanning two orgs + two `ops` repos.

### Orchestration (order matters — never strand a running app)

1. **Guards.** Admin on source **and** destination; destination project exists +
   is onboarded; app not already present in the destination; **no unmerged render
   PR** in *either* project (same gate as rename — env must equal `main` so the
   cutover migrates the deployed state); a monorepo member cannot be moved alone
   (its repo is shared) — refuse, or move the whole repo group.
2. **Image copy.** Copy the app's pinned image digest(s) into the **destination
   org's** GHCR package (`ghcr.io/<dst-org>/<app>`), before anything flips — a
   missing `write:packages` token aborts cleanly. (Mirrors rename step 0, but
   cross-org.)
3. **Repo transfer.** Transfer the GitHub source repo to the destination org
   (`POST /repos/{org}/{repo}/transfer`), archive-not-delete semantics preserved.
   A monorepo repo is not transferred per-member (see guard 1).
4. **Destination ops commit.** Add `apps/<app>/*` (manifests + inline secrets,
   verbatim) to the destination `ops` `main` + add the `apps[]`/`services[]` entry
   to its `project.yaml`, image pin rewritten to `<dst-org>` base.
5. **Freeze** the move in the reconciler keyed on **destination** project+app
   (`rename_add_pending(dst_project, app, app, class)`), before any env branch
   flips, so the drift-poll can't create an empty new stack or GC the old one
   mid-migration.
6. **Source ops commit.** Remove `apps/<app>/*` + the `project.yaml` entry from the
   source `ops` `main`.
7. **Re-render both** projects' `env/*` branches → render PRs; merge the production
   render PR in each (deploy trigger).
8. **Reconciler migrate + commit** (stateful apps): `migrate_stack((src_project,
   app) → (dst_project, app))` copies each named volume + renames the managed DB,
   then clears the freeze so the destination stack converges on the migrated data
   (health-gated) and the old stack is GC'd. Stateless apps just blue-green on the
   destination.
9. **DNS/ingress** reattach to the destination `proj-<dst>` network + reissue
   ingress hosts under the destination domain (production render handles ingress;
   Cloudflare DNS via the bot as today).

### UX

A **"Move to project…"** action in the app-detail ⋯ menu (admin of both), a
destination picker, and a confirm dialog spelling out the brief per-app cutover
downtime — same shape as the rename dialog.

## Consequences

- **Reuses** rename/migrate machinery: `migrate_stack` (already dual-axis),
  `rename_add_pending`/freeze, `commit_ops_tree`, `rewrite_manifest_image`,
  `registry::copy_image`. The genuinely new code is the bot orchestration across
  two org clients + two ops repos.
- **Irreversible + production-affecting on invocation** (repo transfer, live volume
  copy + DB rename, dual re-render with downtime). Ship the capability **inert**
  (like rename/archive/delete) and require an explicit admin action; document that
  the cross-org path must be exercised on a **throwaway app** before first real use.
- **Blast radius if buggy:** a wrong step order could strand a running app between
  projects. Mitigated by (a) the ordering above (destination made ready before the
  source entry is removed), (b) archive-never-delete on the repo, (c) the freeze
  bracketing the git flips, (d) the unmerged-render-PR gate in *both* projects.

## Open questions

- **Monorepo members:** move the whole repo group in one shot, or forbid moving a
  single member? (Leaning: move the group — a shared repo can't live in two orgs.)
- **Partial-failure recovery:** if step 3 (transfer) succeeds but a later step
  fails, the app is half-moved. Need an idempotent re-run / an explicit "resume"
  (the freeze row is the natural checkpoint).
- **Members/teams:** the app's contributors may not be members of the destination
  org — surface a warning; membership is the operator's to reconcile.
- **GHCR package cleanup:** the source-org package is left behind (archive-never-
  delete) — fine, but note it for the storage audit.

## Status / rollout

Design only. Implementation is a single large bot change (+ small reconciler wiring
to expose the project axis of `migrate_stack` over the reconciler API, + the
dashboard action). Ship inert; verify on a throwaway app; then document in the
runbooks. Not yet built — this ADR de-risks that build.
