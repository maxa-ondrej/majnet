---
title: Overview
description: What MajNet is and the ideas it is built on.
---

MajNet is a **self-hosted deployment platform**: GitOps-driven, built on **plain
Docker** with static trust-zoned placement across a small set of nodes, and
organized around **projects**. Each project is its own GitHub organization,
fully managed by the platform.

Two custom Rust services form the control plane:

- **Bot** — the only component that talks to the GitHub and Tailscale APIs. It
  reconciles GitHub org state (repos, teams, members, webhooks), renders app
  manifests, and records releases.
- **Reconciler** — the single orchestrator. It drives each node's Docker API
  directly (over WireGuard, mTLS), provisions databases, and converges the
  declared state onto the nodes.

## Principles

- **Git is the source of truth.** A root *platform* org holds global config; each
  project's `ops` repo holds its app config and SOPS-encrypted secrets. Every
  state change is a commit or pull request — the single imperative exception is
  restart / redeploy-of-the-same-digest.
- **Static placement by trust zone.** Workloads never migrate across security
  boundaries; the node follows deterministically from the environment class.
- **Credential isolation.** The bot and reconciler hold strictly separate
  credentials, so neither can act in the other's domain.
- **Rendering never decrypts secrets.** Secrets pass through encrypted; the
  reconciler decrypts only at deploy time, into tmpfs — never environment
  variables.
- **Archive, never delete.** Removing an app archives its repo; containers and
  stacks are torn down only when their config is gone from git.
- **Images are pinned by digest**, never by tag.

## The one manual step

GitHub does not allow programmatic organization creation, so bootstrapping a
project is: create the org by hand, install the GitHub App, and add one line to
the platform registry. Everything after that is automated.
