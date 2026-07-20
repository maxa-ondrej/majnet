# 0021 — Project-owned "service" apps (external image + config, no repo)

**Status:** accepted · **Date:** 2026-07-21 · relates to [0009](0009-dev-to-ops-delivery.md), [0013](0013-auto-assigned-vpn-ingress-hosts-with-ssl.md), [0014](0014-managed-db-access-via-adminer.md)

## Context

Every MajNet app has assumed a **source repo + CI + the class gradient**
(dev/stable/testing/ephemeral/production). But sometimes you just want to run an
**off-the-shelf, prebuilt Docker image** (a tool, a vendored service, later a
SigNoz-style observability stack) with MajNet's placement + ingress + secrets +
volumes + monitoring, and **none** of the build/release machinery: no repo, no
CI, and one environment rather than a class gradient.

A **manifests-only app** (`apps_post` with `create_repo:false`) already gets most
of the way: it requires an external digest-pinned image, creates no repo, isn't
declared in `project.yaml` (so `org_sync` and the `registry_package` webhook both
ignore it), and its ingress/secrets/volumes/database/render/converge all work.
The only true gap is **placement**: it flows entirely from `EnvClass`
(`common::EnvClass::node_role`: `production`→prod node + public edge; the rest →
private node + tailnet), and env branches carry no node identity. A service that
runs in "one environment on a chosen node" doesn't fit the class model — except
that **the class already encodes exactly what a service must choose: trust zone +
exposure.**

## Decision

Add a first-class **service**: a project-owned app that is an external image +
config, no repo, no CI, running in **one environment chosen by `exposure`**,
which maps to an existing `EnvClass` so all placement/ingress/render/converge is
reused unchanged.

- **`Exposure` → `EnvClass`** (`common/src/project.rs`): `public` → `Production`
  (prod node, Cloudflare edge, a custom domain); `internal` → `Stable` (private
  node, tailnet auto-host, no public exposure). A service is rendered + converged
  as that single class exactly like a manifests-only app — **no reconciler
  change**.
- **Tracked in a `services:` block** on `ProjectConfig`
  (`Vec<ServiceDecl{ name, exposure }>`, `#[serde(default)]`). This records
  ownership + lets the dashboard list/badge services. Because services are **not**
  in `apps:` and have no repo, `org_sync` never creates/archives anything for them
  and the digest webhook never fires — no carve-outs needed.
- **The manifest lives at `apps/<name>/`** (`base.yaml` + the single
  exposure-class overlay) like any app, so edit-image/config, secrets, archive,
  and delete all reuse the existing app paths. Updating a service = editing its
  pinned image digest in git (writes-through-git; the reconciler converges).
- **Creation** (`services::create`, `POST /api/services/{org}`, admin-gated):
  writes the `services:` entry + `apps/<name>/base.yaml` + the exposure-class
  overlay (reusing `dashboard_api::{scaffold_base, scaffold_and_declare,
  commit_file}`) + optional secrets. A public service still gates on its
  `env/production` render PR at deploy time (ADR 0009/0013).
- **Archive/delete** reuse the app paths; archive additionally drops the
  `services:` entry (`app_archive_post`), and delete purges volumes/DBs as usual.

Contrast with **platform-owned** image+config services (DB engines, Adminer —
§15 / ADR 0014), which are hardcoded, platform-placed, and config-hash-converged
outside the `AppManifest` path. A service here is the **project-owned** version:
same "external image + config" shape, but declared per project and run through the
normal app render/converge (blue-green, ingress, secrets, volumes, database).

## Consequences

- Off-the-shelf images run first-class with placement + ingress + secrets +
  volumes + monitoring and zero build machinery — reusing the entire existing
  app pipeline; the only new surface is the `services:` block, a create endpoint,
  and dashboard UX.
- **Single image per service (v1).** A multi-container stack (e.g. SigNoz:
  collector + query + ClickHouse + UI) is composed from several services sharing
  the project network via ADR 0019 aliases — not modeled here.
- An internal service uses the `stable` class branch and coexists with apps'
  stable deployments on the private node (same node/network) — cosmetic only.
- **Follow-on:** the app-detail view still shows the class/deploy/releases/promote
  sections for a service; those are inert/irrelevant for a no-repo service and
  should be hidden (the `service` flag on the app summary drives it).
