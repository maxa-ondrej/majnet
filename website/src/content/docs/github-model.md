---
title: GitHub model
description: One organization per project, fully bot-managed.
---

Each project is its own GitHub organization. GitHub state — repos, settings,
teams, members, webhooks — is fully bot-managed and declarative.

```
majksa-platform (root org)
└── platform            # nodes, people, projects registry, platform manifests

<project> org
├── ops                 # project.yaml, app manifests, SOPS-encrypted secrets
├── <app-1>             # application repo (CI wired for the delivery pipeline)
└── <app-2>
```

## Registry-gated discovery

A project exists when **both** hold: the GitHub App is installed on the org,
**and** the org is listed in the platform `projects.yaml` registry. Installation
alone does nothing — a stray install can't join the platform, and a registry
entry without installation shows as "pending" on the dashboard.

## Fully declarative repos

Apps declared in a project's `ops` repo are materialized by the bot: it creates
missing app repos from a template (GitHub Actions workflow, branch protection,
labels), creates the `ops` repo for newly registered orgs, and continuously
reconciles settings, teams, members, and webhooks against config.

Removing an app from config **archives** its repo — never deletes. Archival is
the safe terminal state.

## Delivery

An app's CI builds and pushes a digest-pinned image to the registry. A
`vX.Y.Z`-tagged publish is recorded as a **release**; a plain `main` build feeds
`testing`; a `pr-N` build feeds an ephemeral preview. Promoting a release pins
its digest into `production` — behind the reviewed render PR.
