# 0013 — Auto-assigned VPN ingress hosts with SSL

**Status:** accepted · **Date:** 2026-07-13 · Phases 1–3 coded; 4 next. 3 unverified until the private node is enrolled

## Context

Migrating `space-alert` (ADR 0010) exposed how ad-hoc non-production ingress
is. Three problems, one root cause — **the platform doesn't own the hostname**:

1. **Hosts are hand-typed and drift.** `space-alert`'s custom production
   domains live in `production.yaml` (correct — they are production-specific).
   But `promote` **rewrote** `production.yaml` as an image-only overlay, so a
   promote would silently drop them. Non-prod is worse: `render_class`
   (`crates/bot/src/render.rs`) merges `base ⊕ overlay` verbatim, so a
   stable/testing app is routed only if it happens to declare a `host` — and if
   it does, that host is arbitrary and unmanaged.

2. **The preview domain is hardcoded.** `crates/bot/src/ephemeral.rs` builds
   `{app}-pr{N}.{project}.majksa.net` from a literal `majksa.net`. There is no
   way to run the platform under another domain.

3. **VPN apps have no real TLS.** The per-project ingress
   (`crates/reconciler/src/ingress.rs`: Traefik + tailscale sidecar) already
   opens a `websecure:443` entrypoint and `deploy.rs` already labels every app
   router `entrypoints=websecure` + `tls=true` — but no browser-trusted
   certificate is ever installed, so Traefik serves its self-signed default. A
   developer hitting `https://app.project.majksa.net` over the tailnet gets a
   cert warning.

The design already anticipates the target shape — §"non-prod lives at
`<app>.<project>.majksa.net` / `<app>-pr<N>.<project>.majksa.net` via split DNS
on the tailnet" — it was just never wired end to end. This ADR does that.

## Decision

### 1. The platform owns non-production hostnames

A single configurable **base domain** replaces the hardcode. It lives in the
platform `nodes.yaml` (already the home of platform-wide networking config —
`wireguard_subnet`, `docker_api_port`):

```yaml
base_domain: majksa.net
```

The bot **assigns** the ingress host for every non-production class at render
time; the app never sets it:

| class     | assigned host                          |
|-----------|----------------------------------------|
| stable    | `{app}.{project}.{base_domain}`        |
| testing   | `{app}.{project}.{base_domain}`        |
| ephemeral | `{app}-pr{N}.{project}.{base_domain}`  |

`Ingress.host` becomes **optional** in the schema. An app opts into routing by
declaring only the port:

```yaml
ingress:
  port: 8080
```

- **Non-production render** (`render.rs`, `ephemeral.rs`): overwrite `host`
  with the assigned value. Any host the manifest carried is ignored for these
  classes.
- **Production stays custom-only** (unchanged): the app declares its real
  `host` + `domains`, which drive Cloudflare + the prod edge (ADR 0007). No
  auto-assignment — a production app with an ingress but no host is simply not
  routed.

The dashboard's manifest/new-app form drops the host field for non-prod and
shows the computed `{app}.{project}.{base_domain}` read-only; the user sets
only the port. Production keeps its editable custom-domains list.

### 2. Browser-trusted TLS for VPN hosts — a per-project wildcard, via DNS-01

VPN hosts are not publicly reachable, so the ADR 0007 prod pattern (Cloudflare
proxy + Origin CA cert, trusted only *behind* Cloudflare) does not apply: a
developer connects **straight to the project's Traefik** over the tailnet, so
that Traefik needs a **publicly-trusted** certificate. HTTP-01 is impossible
(no public reachability), so we use **Let's Encrypt DNS-01 over Cloudflare**.

One **wildcard cert per project** — `*.{project}.{base_domain}` — covers every
app *and* every PR preview in that project (`{app}` and `{app}-pr{N}` are both
single labels under `{project}.{base_domain}`). Issued and renewed by the
**bot**, which already owns the Cloudflare token (ADR 0007) and is the sole
external-API liaison.

The private key crosses to the reconciler the same way ADR 0007 origin keys do
— **age-encrypted in git**:

```
platform/ingress-certs/{project}.crt         # LE wildcard chain, plaintext
platform/ingress-certs/{project}.key.age     # private key, age-encrypted to age-private
```

Reusing `cloudflare.rs::put_platform_file` + `age_encrypt`. The bot touches
ACME + Cloudflare + git; the reconciler touches age + Docker. **Credential
isolation (§6) holds** — nothing new crosses the boundary.

The ACME flow (account, DNS-01 challenge, order/finalize, renewal) is run by
shelling out to **`lego`** — a single Go binary with a native Cloudflare
provider — rather than an in-process ACME crate. This matches the image's
established "shell out to age/openssl/sops" pattern, keeps the fragile,
here-untestable protocol logic out of the bot, and gives battle-tested renewal
for free. `lego`'s account + cert state persists under the bot data volume; the
Cloudflare token reaches it via `CF_DNS_API_TOKEN` (env) and never leaves the
bot. `lego run` issues; `lego renew --days 30` (idempotent) renews inside the
window; the bot re-commits only when the PEM actually changed.

### 3. The reconciler installs the wildcard on each project ingress

`ensure_ingress` gains a cert-delivery step: fetch the platform snapshot,
decrypt `{project}.key.age` (age binary, as the reconciler already does for
edge certs), and deliver `cert.pem`/`key.pem` + a small Traefik **file-provider
dynamic config** onto the ingress container via the `secrets.rs`
helper+`put_archive` mechanism. The dynamic config sets the wildcard as the
`tls.stores.default.defaultCertificate`, so the existing `websecure` + `tls`
router labels resolve to it with no per-app cert work. Recreate Traefik on
cert-hash change (blue-green-ish, like apps and edge-main).

### 4. Split DNS routes the hostname to the project ingress

`{app}.{project}.{base_domain}` must resolve, on the tailnet only, to the
project's ingress. The ingress sidecar already joins the tailnet with
`hostname = {project}` → MagicDNS name `{project}.{tailnet}.ts.net`. The bot
(owner of both Cloudflare and Tailscale) ensures a **DNS-only CNAME**:

```
*.{project}.{base_domain}  CNAME  {project}.{tailnet}.ts.net
```

On-tailnet, MagicDNS resolves the `.ts.net` target to the ingress's `100.x`
address; off-tailnet the CNAME target is unresolvable, so VPN hosts stay
VPN-only with no tailnet IPs published in public DNS. The `{tailnet}` name
comes from the Tailscale API (bot-owned). *(Alternative considered: a Tailscale
split-DNS restricted nameserver. Rejected — it needs a resolver the platform
would have to run; the CNAME-to-MagicDNS trick needs none and keeps both DNS
edits inside the bot's existing Cloudflare + Tailscale ownership.)*

## Phasing

1. ✅ **Base domain + auto-host (no TLS work).** `base_domain` in `nodes.yaml`
   (`NodesFile`, defaulted so pre-0013 files still parse); `Ingress.host`
   optional; `render.rs` + `ephemeral.rs` assign `{app}.{project}.{base_domain}`
   for non-prod and drop any custom host/domains; a port alone opts into routing
   (`scaffold_base`); `deploy.rs` skips the Traefik labels for a host-less
   ingress; `promote` replaces only the `image:` line so it no longer clobbers
   production ingress/env; dashboard copy explains prod-custom vs non-prod-auto.
   Shipped without the private node — **removes the host-drift footgun today.**
2. ✅ **Bot: wildcard cert.** `acme.rs::ensure_ingress_cert(project)` shells to
   `lego` (Cloudflare DNS-01) for `*.{project}.{base_domain}`, age-encrypts the
   key to the production recipient, and commits cert+key to
   `platform/ingress-certs/{project}.{crt,key.age}`; hooked into org-sync
   per synced project (non-fatal), committing only on change, renewing inside a
   30-day window. Config: `MAJNET_ACME_EMAIL` (+ `MAJNET_ACME_STAGING`).
3. ✅ **Reconciler: install the cert.** `ensure_ingress` decrypts
   `{project}.key.age` (age-production key), delivers `wildcard.{crt,key}` +
   a Traefik file-provider config setting `tls.stores.default.defaultCertificate`
   into per-project host dirs (via the `platform::deliver_files` helper), adds
   the file provider + mounts to the ingress Traefik, and recreates it on a
   cert-hash change (`majnet.config-hash` label). No committed cert → Traefik
   still comes up on its self-signed default (untrusted). *Code-complete;
   unverified until the private node runs it.*
4. **Split DNS.** Bot ensures the `*.{project}.{base_domain}` → MagicDNS CNAME.

## Consequences

- **Credential isolation preserved:** the bot gains ACME (an external protocol,
  its existing liaison role); the reconciler gains nothing (already decrypts
  age + drives Docker). The wildcard key crosses only as age-ciphertext in git.
- **GitOps intact:** the base domain, the cert commit, and the render are all
  commits; `git log` stays the deploy history.
- **No more host drift:** non-prod hosts are computed, not stored. Production
  custom domains stay in `production.yaml` where they belong, and `promote` is
  now **non-destructive** — it replaces only the top-level `image:` line
  (reusing `digest::replace_image_line`), so it can never drop a hand-managed
  ingress/env in that overlay again.
- **Depends on the private node** for Phases 2–4 to be observable end to end;
  Phase 1 is independent. Cert issuance failures are non-fatal to convergence
  (degrade to the self-signed default, retried next cycle, surfaced in events),
  mirroring ADR 0007's Cloudflare-failure stance.
- **Renewal** is the bot's job (LE certs are 90-day); it runs in the hourly
  org-sync with a renew-before-expiry window, like the ADR 0007 origin certs.
