# 0028 — Env pins carry only the digest field

**Status:** accepted · **Date:** 2026-07-24 · relates to [0009](0009-dev-to-ops-delivery.md) (delivery/promotion), design §5 (digest-pinning), §8 (config inheritance)

## Context

An app's image reference can be written two ways (the `AppManifest` split-pin
schema): a **combined** `image: ghcr.io/<org>/<app>@sha256:…`, or a **bare
repository** `image: ghcr.io/<org>/<app>` in `base.yaml` paired with a
per-class **`digest:`** in each overlay. The intent of the split is clean
inheritance: `base.yaml` owns the env-unspecific repository, each class overlay
owns only its own pin.

But every writer that pins a digest still emitted the *combined* form into the
overlay via `replace_image_line`:

- `promote.rs` (stable → production) and `releases.rs::production_overlay`
  (promote a release) wrote `image: <repo>@<digest>`.
- `digest.rs::bump_class_digest` (main-build → `testing`; release auto-track →
  `stable`) did the same.

So overlays accumulated a full `image:` that duplicated the repo already in
`base.yaml` and re-pinned it — fighting the split model, and making a promotion
touch more than the one thing it means to move (the digest).

Naively switching the writers to emit `digest:` alone would silently no-op:
`image_ref()` preferred any pin already on `image`, so with a pinned `base.yaml`
`image:` the overlay's `digest:` was ignored.

## Decision

**A class overlay pins only its `digest:`; the repository is inherited from
`base.yaml`.** Two coordinated changes:

1. **`digest` (then `tag`) is authoritative in `image_ref()`** — when set, the
   effective reference is `strip_pin(image)@<digest>`, overriding any pin the
   `image` string itself carries. With neither field set, `image` is used
   verbatim. This lets an overlay's `digest:` win over a pinned `base.yaml`
   `image:`, so promotion works **without** first migrating `base.yaml` to a
   bare repo.

2. **Every digest writer emits `digest:`, never a combined `image:`.** A shared
   `replace_digest_line` sets/replaces the top-level `digest:` field, preserves
   comments and hand-managed keys (custom ingress, env — ADR 0013), and demotes
   any stale combined `image:` pin in the overlay to its bare repo so the
   on-disk overlay isn't misleading. `promote.rs` reads whatever `stable` runs
   (`overlay_digest`: a `digest:` field, else the pin on a legacy `image:`) and
   copies that digest into `production`.

## Mechanics

- `common/manifest.rs`: `image_ref()` reordered — `digest`/`tag` first, both
  applied over `strip_pin(&self.image)`; new `strip_pin()` (leaves a registry
  `host:port` intact, strips a real `:tag`/`@digest`).
- `bot/digest.rs`: new `replace_digest_line`, `overlay_digest`, `digest_of`;
  `bump_class_digest` writes the digest field. `replace_image_line` removed
  (services keep their own copy in `service_releases.rs`, which pins an upstream
  `image:` in `base.yaml` and is intentionally out of scope).
- `bot/promote.rs`, `bot/releases.rs`: write `digest:`; new/minimal overlays are
  `digest: <sha>` only.
- `reconciler/info.rs`, `reconciler/converge.rs`: read `manifest.image_ref()`
  instead of raw `manifest.image` (correct under split overlays — OCI-label
  lookup and the deploy-event version label).

## Consequences

- **Byte-compatible, no fleet recycle.** No manifest currently sets
  `digest`/`tag`, so `image_ref()` returns exactly what it did for every
  existing manifest; `config_hash` is unchanged fleet-wide.
- **Migration is on-write.** A legacy `production.yaml` keeps its combined
  `image:` until its next promote/bump, then flips to the digest-only form. No
  mass rewrite; `base.yaml` may keep a pinned `image:` indefinitely (the
  overlay's digest wins).
- A promotion diff now shows a one-line `digest:` change — clearer review, and
  the promotion literally moves only the digest.
- **Services unchanged** — a service has no bare-repo split (its upstream image
  is pinned in `base.yaml`), so `service_releases::promote` still rewrites
  `image:` there.

## Alternatives rejected

- **Migrate every `base.yaml` to a bare repo up front** (so a plain `digest:`
  overlay resolves): a fleet-wide config churn (`config_hash` change → recycle)
  for no gain over making `digest` authoritative, which is byte-compatible.
- **Write `digest:` but leave the stale combined `image:` in the overlay:**
  correct at runtime (digest wins) but the overlay would show an old digest
  prominently next to the new one — misleading on disk.
