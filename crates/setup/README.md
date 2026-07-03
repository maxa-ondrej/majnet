# majnet-setup

The provisioner (ADR 0004): first-run wizard + node enrollment. Installed and
started by `bootstrap/install.sh` on the main node; never runs anywhere else.

**Credentials held:** enrollment SSH key (`enroll_ed25519`), PKI CA
(`pki-ca/`), one-time wizard token. No GitHub, no Tailscale, no age keys, no
Docker client certs — the fourth disjoint credential class (§6).

## Configuration (env)

| Variable | Default | Purpose |
|---|---|---|
| `MAJNET_SETUP_LISTEN_PUBLIC` | `0.0.0.0:7600` | wizard — first run only, closed by /finish |
| `MAJNET_SETUP_LISTEN_INTERNAL` | `127.0.0.1:7601` | enrollment API — **bind to the WG IP** in production |
| `MAJNET_ETC_DIR` | `/etc/majnet` | token, state, bot.env, PEM, PKI CA, done marker |
| `MAJNET_REPO_DIR` | `/opt/majnet` | majnet checkout (`bootstrap/` payload + `platform-seed/`) |
| `MAJNET_BOT_INTERNAL_URL` | `http://10.88.0.1:8081` | bot platform API (seed, node upsert) |

## Flow

1. `install.sh` prints `http://<ip>:7600/?token=<setup-token>`.
2. Wizard: basics → GitHub App (manifest flow → the bot's credentials are
   written to `bot.env` + `github-app.pem`, bot restarted) → install the App
   on the root org → seed the `platform` repo (committed by the **bot**) →
   enroll workers → finish (public listener closes for good).
3. Enrollment (`POST /enroll` — also on the WG-internal listener, forever):
   push `bootstrap/`, render `node.env`, install PKI server certs, run
   `bootstrap.sh` as root, then switch to the `majnet` admin user (10-base
   disables root SSH), collect the WG pubkey, re-render peers on every node
   (`bootstrap.sh 10 20`), register in `nodes.yaml` via the bot.

The `bootstrap/` scripts remain runnable standalone — break-glass unchanged.
