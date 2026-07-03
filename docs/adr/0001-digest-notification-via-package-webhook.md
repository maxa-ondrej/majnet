# 0001 — Digest notification via `registry_package` webhook

**Status:** accepted · **Date:** 2026-07-03

## Context

The design (§11.4, §13) has GHA notify the bot with `(org, app, digest)` after
pushing an image. A custom HTTP call from the workflow to the bot would need
an authentication story (per-org shared secrets, or verifying GitHub OIDC
tokens against JWKS) and a reachable custom endpoint.

## Decision

Use GitHub's native **`registry_package` webhook** instead: a GHCR publish in
a project org fires an event to the GitHub App, HMAC-signed with the same
webhook secret as every other event. The payload carries the org, package
name (= app name), tag, and image digest. Workflows just build and push —
no callback step, no extra credentials.

Tag conventions carry intent: `latest`+`sha-*` on main → stable digest bump;
`pr-<N>` → ephemeral flow (phase 4).

## Consequences

- One verification path for all inbound events; nothing to rotate per org.
- The GitHub App must subscribe to *package/registry package* events, and app
  repos' packages must belong to the org (GHCR default for org repos).
- Digest extraction depends on the `registry_package` payload shape
  (`container_metadata.tag.digest`, falling back to `package_version.version`)
  — verify against a real delivery during phase-1 rollout; the parsing lives
  in `crates/bot/src/digest.rs`.
