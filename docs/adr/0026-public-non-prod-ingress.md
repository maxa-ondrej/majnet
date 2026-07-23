# 0026 — Public ingress for non-production classes (exploration)

**Status:** proposed / exploratory — not yet decided · **Date:** 2026-07-23 · relates to [0007](0007-reconciler-owned-platform-services.md), [0013](0013-auto-assigned-vpn-ingress-hosts-with-ssl.md); touches design §4 (topology), §7 (networking), §8 (environment classes)

> This ADR captures an exploration, not a commitment. It exists so the analysis
> isn't lost. No code changes follow from it until a model is chosen.

## Context

A non-production class (`stable`, e.g. sideline's `dev.sideline.cz`) is, by design,
reachable **only over the tailnet**. This blocks anything that needs a real public
origin — most concretely **third-party OAuth callbacks** (Discord, GitHub, …), which
won't call back to a `*.ts.net`/split-DNS host. That is the practical driver for
wanting a non-prod env to be publicly reachable.

"Public" is not a flag today — it's derived from **static, trust-zoned placement**:

- `EnvClass::node_role()` (`crates/common/src/lib.rs`) hardcodes
  `Production → prod`, `Stable | Testing | Ephemeral → private`. Publicness follows
  from *which node* an app lands on.
- Only the **prod** node runs the public edge: `edge-main` (Traefik) is deployed by
  the reconciler onto `nodes.by_role("prod")` only (`crates/reconciler/src/platform.rs`),
  bound to `0.0.0.0:80/443`, with Cloudflare **Origin-CA** certs (Full-strict) and a
  firewall that admits `80/443` **only from Cloudflare ranges** (`bootstrap/steps/40-firewall.sh`).
- The **private** node has **no public HTTP**: each project's Traefik runs inside a
  Tailscale sidecar's netns (no host port binding, `crates/reconciler/src/ingress.rs`),
  and the firewall opens **only** `22` + WireGuard `51820` — *"private: no public
  service ports at all."* The bot publishes a DNS-only wildcard CNAME
  `*.<project>.<base_domain>` → the project's MagicDNS name, which is unresolvable
  off-tailnet by design (ADR 0013: DNS-01 issuance, split-DNS, "VPN hosts stay
  VPN-only").

So the current behaviour is a deliberate **trust-zone boundary** (design §4/§7/§8),
not an accident — but it is also **not** an explicit security *prohibition* with a
stated threat model. Making a non-prod class public is "not built and cuts against
the topology," and would require, at minimum: (a) a placement/role opt-in, (b) a
public-bound edge listener on the private node, (c) opening `:443` in its firewall,
(d) proxied DNS → the private node's public IP + an Origin cert for the host.

## Options considered

### A. Cloudflare Tunnel (cloudflared) on the private node — *leading candidate*
Run a `cloudflared` sidecar on the private node (per-project or per-node). It dials
**outbound** to Cloudflare; Cloudflare routes the public hostname through the tunnel
to the project's existing Traefik. 
- **No inbound ports opened** — the private-node firewall stays closed (`:443` never
  exposed); the node is *not* made a public-facing box.
- Reuses the bot's existing Cloudflare integration (sideline's zone is already on
  Cloudflare). **Free** — `cloudflared` is OSS and public-hostname tunneling is on
  Cloudflare's free plan (verify at build time).
- New moving part: a `cloudflared` component + a tunnel credential (bot-minted, like
  the Tailscale auth key). TLS terminates at Cloudflare (or CF→Traefik origin cert).
- Still publishes non-prod publicly — the trust-zone erosion is scoped to the one
  app's hostname, not the node.

### B. Public edge on the private node (mirror prod)
Deploy an `edge-main`-style public Traefik on the private node, open `:443` (CF
ranges) in its firewall, point a **proxied A record → the private node's public IP**,
issue an Origin cert for the host. Behaves exactly like prod.
- **Largest blast radius:** turns the private node — which also holds **databases**
  and *other projects'* non-prod apps — into a public-facing host. Directly inverts
  the "no public service ports on private" firewall stance and the §4 topology.

### C. Tailscale Funnel
Expose the tailnet service publicly via Tailscale's edge.
- Only serves `*.ts.net` names on a few fixed ports; **no clean custom-domain cert**
  (a CNAME to the funnel name mismatches the cert). Doesn't satisfy `dev.sideline.cz`.
  Rejected for the custom-domain use case.

### D. Keep VPN-only (status quo)
Reach non-prod via the wildcard tailnet host over Tailscale; drop custom public
domains for non-prod. No posture change. Does **not** unblock public OAuth callbacks.

## Leaning (not decided)

If we pursue this, **Option A (Cloudflare Tunnel)** is the least-invasive: it unblocks
public reachability (and OAuth) for a chosen non-prod host without opening the private
node's firewall or turning it into a public node, and it reuses Cloudflare + a
bot-minted credential (mirroring the Tailscale-authkey pattern). It would still be a
real design change — a new reconciler-owned component, a bot credential, and an opt-in
on the manifest/class (e.g. `public: true` on a stable overlay) — and would warrant
promoting this ADR to *accepted* with a concrete design + a note on the trust-zone
implication (non-prod, auto-deployed code becomes internet-facing).

## Open questions

- Opt-in granularity: per-class (`stable` only) vs per-app manifest flag.
- Who terminates TLS (Cloudflare edge vs origin cert to the project Traefik)?
- Does public non-prod need Cloudflare Access in front (auth) by default?
- Interaction with the bot's current `production_hosts`-only Cloudflare automation.
