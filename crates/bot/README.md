# majnet-bot

The liaison (design doc §11). Phase-1 MVP: GitHub App auth, webhook intake, digest bumps, repo snapshot proxy, reconciler notify.

## Configuration (env)

| Variable | Required | Default | Purpose |
|---|---|---|---|
| `MAJNET_GITHUB_APP_ID` | ✱ | — | GitHub App ID |
| `MAJNET_GITHUB_PRIVATE_KEY_PATH` | ✱ | — | App private key PEM |
| `MAJNET_WEBHOOK_SECRET` | ✱ | — | webhook HMAC secret |
| `MAJNET_LISTEN_WEBHOOK` | | `0.0.0.0:8080` | public webhook listener (via edge/tunnel) |
| `MAJNET_LISTEN_INTERNAL` | | `127.0.0.1:8081` | snapshot API — **bind to the WG IP** in production |
| `MAJNET_RECONCILER_URL` | | *(empty = off)* | reconciler notify endpoint over WG |
| `MAJNET_DATA_DIR` | | `/var/lib/majnet-bot` | SQLite + snapshot cache |
| `MAJNET_ROOT_ORG` | | `majksa-platform` | root platform org |
| `MAJNET_TAILSCALE_API_KEY` | | *(empty = TS sync off)* | the bot's second credential (§6) |
| `MAJNET_TAILNET` | | — | tailnet name (e.g. `example.com`) |

## GitHub App settings

- **Webhook URL** → the public listener's `/webhook`; secret = `MAJNET_WEBHOOK_SECRET`.
- **Events:** push, pull request, registry package (ADR 0001).
- **Permissions:** contents RW, pull requests RW, administration RW (phase 3), members RW (phase 3), packages R.

## Endpoints

| | |
|---|---|
| `POST /webhook` | GitHub events (HMAC-verified, delivery-deduped) |
| `GET /api/snapshot/{org}/{repo}/{branch}` | internal: branch tarball + `X-Majnet-Commit` (reconciler only) |
| `POST /api/tailscale-authkey/{project}` | internal: mint a one-shot tagged auth key for a project ingress |
| `GET /healthz` | on both listeners |

## Org reconciliation (phase 3)

Hourly + on every platform/ops `main` push: registry-gated discovery (App installed ∧ listed in `projects.yaml`), ops repo creation with scaffold, app repos from `repo-templates/<t>/` (placeholders `{{app}}`, `{{org}}`), archive-on-removal, branch protection (`env/production` requires review — the production gate; app `main` requires the `test` check), `admins`/`developers` team sync, Tailscale ACL render + push.
