# 0024 — Inline per-key encrypted secrets

**Status:** accepted (phases 1–2 implemented) · **Date:** 2026-07-22 · relates to [0007](0007-custom-domains-cloudflare.md), [0010](0010-app-migration-from-external-paas.md), [0012](0012-node-registry-auth-for-private-images.md); revises design §14

## Context

Until now an app's secrets lived in a **separate SOPS file per class** —
`apps/<app>/secrets.<class>.yaml` in the project's `ops` repo — while the app
manifest carried only `secrets: Vec<String>` (a bare **allowlist of names**). That
is a second file to find, wire up (`.sops.yaml` recipient rules) and keep in sync
with the manifest.

This folds secrets **inline into the config file**, one encrypted line per key —
conceptually a Kubernetes `Secret.data` map, but the value is actually encrypted:

```yaml
secrets:
  DATABASE_URL: majnet:AgV1...      # single-line encrypted blob
  SESSION_SECRET: majnet:AgB9...
```

Secrets are now discoverable next to the config they belong to, trivial to add
(encode-and-paste — no `sops -e -i` / `.sops.yaml` dance), and reviewable per line
in diffs, while preserving the crypto asymmetry: **anyone can encode** (public
recipient), **only MajNet decodes** (the reconciler holds the private key).

## Decision

- **Envelope:** `majnet:<base64(age ciphertext)>` — one line per value. Encode
  with `age -r <class recipient>` (binary, base64-wrapped); decode with `age -d -i
  age-<class>.key`. Reuses the `age` binary already used for cert keys (ADR 0007);
  **drops SOPS for app secrets**.
- **Decrypt model = platform-key-only.** Each value is encrypted **only** to the
  platform class recipient (`age-production` / `age-stable`). The reconciler is the
  sole decryptor; admins view/edit exclusively through the dashboard (VPN,
  admin-gated). This **revises design §14**, which also listed project-admin keys as
  recipients — admins lose local `age -d`, gaining tighter isolation.
- **Schema:** `AppManifest.secrets` is a `Secrets` enum accepting either the new
  `Inline` map or the legacy `Names` list (distinguished by YAML shape). Default =
  empty legacy list (`secrets: []`), byte-identical to the pre-0024 field so the
  reconciler's manifest-serialized `config_hash` is unchanged — **no fleet recycle**.
  Overlay merge treats the inline map per-key (base ⊕ class overlay merge/override/
  `null`-delete).
- **Cutover = compat-then-migrate.** The reconciler treats the two shapes as
  mutually exclusive with **inline authoritative**: an inline map fully defines the
  set (any stale SOPS file ignored); otherwise the legacy SOPS file is used. Existing
  apps keep running; the SOPS pass-through is removed only once all are migrated.

## Invariants preserved (design §6/§14)

Rendering never decrypts (the `majnet:` ciphertext travels inside the rendered
manifest); the reconciler is the only decryptor (credential isolation); delivery
stays **tmpfs files at `/run/secrets/<KEY>`, never env vars**; a value re-encrypts
per key on recipient rotation.

## Migration (isolation-preserving)

The bot cannot decrypt. To convert a legacy app the **reconciler** decrypts the SOPS
file, re-encrypts each value to the class recipient, and returns the **ciphertext**
map (never plaintext) to the bot, which commits it inline and deletes the SOPS file.
Editing secrets via the dashboard also converts naturally (read decrypted → save
inline). Class-key rotation reuses the same re-encrypt sweep (replacing `sops
updatekeys`).

## Rollout

1. **Schema + reconciler read** *(done)* — `Secrets` enum + validation;
   `secrets::decrypt_inline` (raw `age`, per-class key); converge reads inline (else
   legacy SOPS); byte-compatible serialization → no recycle.
2. **Bot encode inline** *(done)* — `set_app_secrets` encrypts each value with `age`
   to the class recipient and writes the inline `secrets:` map to the class overlay;
   render no longer requires a SOPS file for inline apps; the dashboard read endpoint
   decrypts inline (base ⊕ overlay) or falls back to the SOPS file. New bot config
   `MAJNET_AGE_STABLE_RECIPIENT` (production recipient already existed).
3. **Migrate + retire SOPS** *(done)* — a reconciler re-encrypt endpoint + bot
   `.../migrate` converted every legacy app (7 of 20 had SOPS files) to inline, then
   the render SOPS pass-through, `.sops.yaml` seeding, legacy `secrets::decrypt`, and
   the migration endpoints were all removed. No SOPS files remain; converge/read are
   inline-only, and a stray legacy bare-name declaration now fails the deploy loudly.
4. **Local encode + docs** *(done)* — **`GET /api/secrets/recipients`** (public,
   unauthenticated, on the public listener) returns the platform's public age
   recipients so a developer can encode a value **locally** — plaintext never leaves
   their machine:
   ```sh
   printf %s "$VALUE" | age -r <recipient> | base64 -w0 | sed 's/^/majnet:/'
   ```
   Encode-only by construction (a public key can't decrypt; the private-key path is
   never exposed). Runbook updated.

## Editing / encoding paths (end state)

- **Dashboard** (VPN, admin-gated for prod/base): the Configuration sheet's per-file
  Secrets editor → bot `set_app_secrets` `age`-encrypts to the file's recipient(s)
  and writes the inline map. `base.yaml` → all recipients; `<class>.yaml` → that class.
- **Local** (anyone): `GET /api/secrets/recipients` + the `age` one-liner above.
- **Decrypt**: reconciler only, at deploy, into tmpfs. Never exposed.
