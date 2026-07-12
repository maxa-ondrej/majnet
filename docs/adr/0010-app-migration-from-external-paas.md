# ADR 0010 — App migration: importing apps from an external PaaS

**Status:** proposed
**Date:** 2026-07-12

## Context

Apps already exist elsewhere: **source on GitHub**, **running on a third-party
PaaS** (Heroku / Dokku / Coolify / CapRover / …) with environment variables and
a database. Onboarding one by hand today means: copy the repo + add MajNet CI,
hand-author `base.yaml`, re-enter every env var as a SOPS secret, and
dump/restore the database. This ADR adds a guided, operator-driven **importer**
as an "Import existing" mode in the New-app flow.

Two hard invariants shape the whole design:

- **Writes go through git.** The importer produces commits/PRs to the app repo
  and the ops repo; it never imperatively deploys. The reconciler converges as
  usual. The single exception is the one-shot **data restore** on the node
  (data is not git-shaped) — see below.
- **Credential isolation.** We do **not** grant the bot or reconciler standing
  access to the old PaaS. The operator produces an **export bundle** from the
  old side and uploads it. The bundle then splits by destination along the
  existing credential line: **code + secrets → bot** (GitHub API + SOPS encrypt
  with *public* recipients), **data → reconciler** (over WireGuard, restored
  on-node).

## Decision

### An operator-produced migration bundle (no live PaaS connection)

The first target PaaS is **Coolify** (self-hosted; per-service env + a managed
database). The operator runs `majnet-export` — a small script we ship, tailored
to the source PaaS — which emits:

```
bundle/
  bundle.yaml            # app, source PaaS, repo URL, env keys, data manifest
  env                    # KEY=VALUE (plaintext in the bundle; never committed as-is)
  data/db.dump           # pg_dump / mysqldump / … (optional)
  data/volumes/<n>.tar.gz  # volume archives (optional)
```

`bundle.yaml`:

```yaml
app: blog
source: coolify                 # provenance only
repo: https://github.com/old-org/blog
env_keys: [DATABASE_URL, SECRET_KEY, …]   # keys present in ./env
database:                                  # optional
  engine: postgres
  dump: data/db.dump
volumes:                                   # optional
  - { name: uploads, path: /app/uploads, archive: data/volumes/uploads.tar.gz }
```

The env **values** live in the bundle (plaintext) but are never committed as-is:
the bot encrypts them (below). The bundle splits into two upload targets so
sensitive/large data never flows through git or the bot.

### Import mode in New-app (bot)

The "New app" form gains an **Import existing** toggle. The operator supplies
the old repo URL (+ a read token for a private source, held only in memory —
never persisted or committed) and uploads the **env** part of the bundle. The
manifest still comes from the form fields (image, ports, domains, database) —
migration does **not** try to reverse-engineer `base.yaml`. Because a repo copy
is slow (and GitHub's source-import is async), the import runs as a **background
task**; progress lands in the events feed. It:

1. creates an empty destination repo and **imports the old repo** via the GitHub
   **source-import API** (server-side — handles full history + binaries, which
   the git-data blob path can't). Normalizes the default branch to `main`;
2. **injects `build.yaml` / `release.yaml`** from the chosen template's
   `.github/workflows/` (placeholders substituted) as one commit on `main`, and
   applies branch protection (mirrors org-sync);
3. **scaffolds `base.yaml`** from the form (the existing `apps_post` path);
4. **encrypts env → `secrets.<class>.yaml`**: shells out to `sops --encrypt`
   with the recipients from ops `.sops.yaml`. Encryption needs only the age
   **public** keys, so isolation holds (the reconciler still owns the private
   keys and is the only decryptor, §14);
5. **declares the app in `project.yaml`** *after* the repo exists, so org-sync
   sees the repo and skips its template-scaffold path → render PRs follow.

### Data restore (reconciler, one-shot)

Data is not git-shaped and is sensitive, so it bypasses the bot and git. The
operator uploads the DB dump (raw request body) to
`POST /api/migrate/{project}/{app}?class=&engine=` on the reconciler over
WireGuard — WG-trust-gated like the bot's snapshot API (operator-on-the-node,
not a per-user dashboard action). The reconciler:

1. provisions the engine if needed (existing `platform::ensure_engine`),
2. provisions the app's DB + user (existing `db::ensure`),
3. restores the dump into that DB with the engine's native client as superuser,
4. records completion so it is **idempotent** (a re-upload is a no-op).

This is the one imperative step; it is coordinated with the first production
deploy and a cutover runbook (DNS handoff, maintenance window).

**Volumes** are out of scope for now: the manifest has no `volumes` field — apps
are stateless-except-DB — so a restored volume would have no mount target.
Volume migration waits on first adding volume support to the core manifest.

## Phasing

1. ✅ **Repo + CI import** — "Import existing" copies the old repo + injects CI;
   manifest + `project.yaml` from the existing `apps_post`. (bot)
2. ✅ **Secrets import** — env (dotenv) → `sops --encrypt` (recipients from ops
   `.sops.yaml`) → `secrets.<class>.yaml` for the target class, declared in that
   class overlay. Delivered as tmpfs files, never env vars (§14). (bot)
3. ✅ **Data restore** — `POST /api/migrate/{project}/{app}` restores a DB dump
   into the provisioned engine (postgres + mariadb SQL dumps), idempotent.
   Volumes deferred (no manifest volume support); Mongo/Valkey later. (reconciler)
4. **Export helper + runbook** — `majnet-export` for the specific source PaaS;
   the cutover checklist.

## Resolved

- **Source PaaS:** Coolify first (the export helper is PaaS-specific; the bundle
  format is not).
- **Secrets:** bot-side `sops --encrypt` with the public recipients from
  `.sops.yaml` — the bot handles plaintext values only transiently, in memory.
- **Data path:** direct WireGuard upload to the reconciler.
- **Cutover:** maintenance-window (stop writes → final dump → restore → deploy →
  switch DNS); zero-downtime is out of scope for v1.
- **Repo import:** full history via the GitHub source-import API (the git-data
  blob path can't carry binaries); default branch normalized to `main`.

## Open items

- Import failure recovery: the background task is fire-and-forget (bot restart
  loses it). v1 logs start/failure events; the operator retries. A persisted
  job/reconcile is a later hardening.
- Bootstrap image for an imported app before its first MajNet CI build (the form
  requires a digest-pinned image): use the old image if pullable, else a
  placeholder until CI publishes.
