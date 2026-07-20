# 0022 — Observable release progress

**Status:** accepted · **Date:** 2026-07-21 · relates to [0009](0009-dev-to-ops-delivery.md), [0020](0020-per-app-monorepo-releases.md)

## Context

Cutting a release (single, bulk, or autorelease) is fire-and-forget: the bot
commits the version bump + changelog, creates the tag, and returns a one-line
toast. Everything after — CI building the image, the `registry_package` webhook
recording it, `stable` auto-tracking — happens invisibly. An operator gets no
signal about *where* a release is or *why* it stalled (a stuck CI build, a
missed webhook), and bulk releases (N apps at once) compound the opacity.

The store already records the finished release row (keyed `(org, app, version)`),
and the import feature (ADR 0010) already models a live per-entity stepper
(`imports` table → `set_import`/`fail_import` → `ImportSteps` UI). Release
progress is the same shape for the pre-deploy pipeline.

## Decision

Add a per-release **progress record** advanced through the pipeline stages:

```
committing → tagging → building → published → tracked
```

- **Store.** A `release_progress` table keyed `(org, app, version)` (version
  normalized to bare — no leading `v` — so the cut-time key and the webhook-time
  key match regardless of tag spelling), columns `status` (`active|done|failed`),
  `stage`, `detail`, `updated_at`. `set_release_stage` upserts (status derived:
  `done` at `tracked`, else `active`); `fail_release_stage` keeps the stage.
- **Transitions.** `do_cut` / `submit_draft` / `submit_repo_group` seed
  `committing`, then `tagging` (after the bump/changelog commit), then `building`
  (after the tag is created — the bot's last action; it stays here until CI
  publishes). `digest.rs::on_package_published` → `record` sets `published`
  (image recorded) then `tracked` (after `track_stable`). A tag-creation failure
  marks the stage failed.
- **Endpoint.** `GET /api/releases/progress/{org}` (static prefix — avoids the
  matchit collision with `/api/releases/{org}/{app}`) returns the org's active +
  recently-terminal rows, newest first; terminal rows older than a TTL are pruned
  on read.
- **Dashboard.** A `RELEASE_STAGES` const + a stepper (adapted from `ImportSteps`)
  on the `/releases` page, live-updating via `refetchInterval`, one per in-flight
  release; bulk releases show all their rows.

## Consequences

- An operator watches a release land (and sees a stuck CI build or a missed
  webhook) instead of guessing. Especially valuable for bulk releases.
- Progress is best-effort + cosmetic: a failed progress write never breaks a
  release. Terminal rows are GC'd after a TTL.
- Mirrors the import-status precedent end to end (table → store → endpoint →
  stepper), so it reuses a proven shape.
- Post-deploy states (migration, health-gate, Traefik flip) remain future work
  (the "deploy trackability" backlog item).
