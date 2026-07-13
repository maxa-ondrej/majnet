---
title: Architecture
description: Nodes, trust zones, and the control-plane flow.
---

## Why plain Docker

Topology is static placement by trust zone: every service has exactly one home
node. A dynamic scheduler would be constrained into doing nothing, and its
headline feature — auto-rescheduling across nodes — is actively unwanted (prod
workloads must never float onto the dev node). Dropping Swarm/Kubernetes removes
Raft/quorum/overlay networking and makes the reconciler the single orchestrator.

| Scheduler feature | MajNet replacement |
|---|---|
| Scheduling | Static: node follows from the environment class |
| Overlay networks | Per-project Docker networks per node; WireGuard cross-node |
| Cluster secrets | Reconciler injects secrets as tmpfs files at container create |
| Rolling updates | Blue-green: start new → health-check → flip Traefik label → stop old |
| Service discovery | Traefik label-based routing per node |

## Nodes & trust zones

| Node | Trust zone | Runs |
|---|---|---|
| **main** | control plane | bot, reconciler, dashboard, observability |
| **prod** | public workloads | `edge-main` (Traefik), production apps + databases |
| **private** | internal workloads | per-project ingresses, stable/ephemeral apps, dev databases |

Data stays inside its trust zone. Node recovery is a bootstrap script + restore
from backup + reconverge from git — there is no state the git repos don't hold.

## The mesh

- **Machines** connect over plain **WireGuard** — three static peers. Each node's
  Docker API listens only on its WireGuard IP, with mTLS client certs held by the
  reconciler.
- **People** connect over **Tailscale** — groups and ACLs; each project's ingress
  joins the tailnet so members reach only their project's apps.

## Deploy flow

1. A change lands on a project's `ops` repo `main` (via the dashboard or a PR).
2. The bot renders `base.yaml` ⊕ the class overlay into the `env/<class>` branch,
   opening a render PR (production waits for an admin review — that review *is*
   the production gate; lower classes auto-merge).
3. The reconciler fetches the rendered manifests and converges each node's Docker
   API: networks, databases, secrets (tmpfs), then a blue-green container rollout
   behind health-checked Traefik routing.
