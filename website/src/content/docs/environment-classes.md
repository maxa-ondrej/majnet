---
title: Environment classes
description: How production, stable, testing, and ephemeral differ.
---

An app is described by a shared `base.yaml` merged with thin per-class overlays.
The **class** determines where it runs and how it deploys — placement is never a
scheduling decision.

| Class | Node | Access | How it deploys | Lifetime |
|---|---|---|---|---|
| `production` | prod | public (custom domains, Cloudflare edge) | reviewed render PR — **admin merge required** | permanent |
| `stable` | private | VPN | auto-deploy from the latest tagged release (`vX.Y.Z`) | permanent |
| `testing` | private | VPN | auto-deploy from the latest `main` build | permanent |
| `ephemeral` | private | VPN | per pull request, generated from the base + a PR patch | TTL after PR close |

## Opt-in by overlay

An app runs a class only if it commits that class's overlay (`stable.yaml`,
`production.yaml`, …). An absent overlay simply means the app doesn't run there —
the platform never invents one.

## Ingress hosts

Non-production classes get an **auto-assigned** host `{app}.{project}.<base-domain>`
(previews get an `{app}-pr{N}.…` name) served over the VPN — the app only
declares a port. Production uses real custom domains wired through Cloudflare and
the public edge.

## The production gate

Testing and stable render PRs auto-merge, preserving auto-deploy. The
`env/production` render PR waits for an admin review of the exact final diff —
that review *is* the production gate. Because class policies are enforced by
branch protection on the `ops` repos, even a compromised dashboard cannot skip a
production review.
