# Runbook — migrating an app from Coolify (ADR 0010)

Moves an app whose **source is on GitHub** and which **runs on Coolify** onto
MajNet: repo + CI, environment → SOPS secrets, and database data. Cutover is a
**maintenance window** (stop writes → final dump → restore → switch DNS).

MajNet never connects to the old host — you produce an export bundle and hand
its parts to MajNet along the credential-isolation line (code + env → the bot;
the DB dump → the reconciler over WireGuard).

## Prerequisites

- The app's GitHub repo URL (and a read PAT if it is private).
- The target **project** exists in MajNet.
- The project's ops `.sops.yaml` has recipients for the target class, or secret
  import fails with a clear error (see `secret-rotation.md`). Fresh ops repos
  scaffold `creation_rules: []` — add the class key + admins first.
- SSH to the old Coolify host (for the export), and a WireGuard-connected host
  for the data restore (e.g. the main node).
- Only **postgres** and **mariadb** are supported in v1.

## 1 · Export from the old host

On the Coolify host (it talks to the local Docker daemon):

```sh
./majnet-export --app blog --engine postgres \
  --db-container <coolify-db-container> \
  --app-container <coolify-app-container> \
  --out ./bundle
```

Produces `./bundle/blog.env` and `./bundle/blog.dump`. **Review `blog.env`** —
prune base-image defaults (`PATH`, `LANG`, …); keep app config + secrets.
Everything left is imported as SOPS-encrypted secrets.

## 2 · Import repo + env (dashboard)

**New app → "Import existing":**

- **Old repo URL** (+ read token if private).
- Fill **image / port / domains / classes / database** as for any new app (the
  manifest comes from the form; migration doesn't reverse-engineer it).
- **Environment variables:** paste the reviewed `blog.env`.

Submit. The bot imports the repo (history preserved) + injects MajNet CI,
scaffolds the manifest, encrypts the env into `secrets.<class>.yaml`, and
declares the app. Watch the notifications feed for **`app-import-done`**
(`app-import-failed` carries the reason).

## 3 · First deploy (empty DB)

Trigger a build from the imported repo (push `main` for testing, or tag
`vX.Y.Z` for a release), then promote / merge the render PR as usual. Confirm
the app runs against an empty database.

## 4 · Data cutover (maintenance window)

1. **Stop writes** on the old app (maintenance mode / scale to 0).
2. **Final dump** — re-run `majnet-export` (or just its dump step).
3. **Restore into MajNet**, from a WireGuard-connected host:

   ```sh
   curl -fsS -X POST \
     "http://10.88.0.1:9090/api/migrate/<project>/blog?class=production&engine=postgres" \
     --data-binary @./bundle/blog.dump
   ```

   Idempotent — a second POST is a no-op. A **partial** failure (e.g. the dump
   errored halfway) needs a manual `DROP DATABASE` / `CREATE DATABASE` of the
   app's DB on the engine before retrying.
4. **Switch DNS** to MajNet for the production domain (Cloudflare + origin cert
   are handled by the platform, ADR 0007).
5. Verify end-to-end, then **decommission** the old Coolify app.

## Notes & limits

- **Secrets are tmpfs files, never env vars** (§14). If the app read config from
  `process.env`, adapt it to read the secret files under its secrets dir, or
  move non-secret config into the manifest's `env:`.
- **Volumes are not migrated** — the manifest has no `volumes` field yet (apps
  are stateless-except-DB), so there is no mount target. Apps relying on local
  disk need that support added first.
- **Mongo / Valkey** data restore is not supported in v1 (Mongo needs
  archive+namespace remapping; Valkey is a cache).
