//! Cloudflare API client (ADR 0007) — the bot's third external credential.
//!
//! Automates the edge wiring for custom domains: for a production hostname it
//! finds the delegated zone, points a **proxied** DNS record at the prod node,
//! and sets the zone to Full (strict). Origin CA certificate issuance is a
//! separate step (see `origin_cert`). The reconciler never sees the token —
//! credential isolation (§6) holds.

use anyhow::{bail, Context, Result};
use majnet_common::manifest::AppManifest;
use majnet_common::platform::NodesFile;
use serde::Deserialize;
use std::collections::BTreeMap;

use crate::AppState;

const API: &str = "https://api.cloudflare.com/client/v4";

/// Ensure the Cloudflare edge wiring (proxied DNS → prod, Full-strict) for
/// every production hostname in a freshly rendered `env/production` tree.
/// No-op without a token. Per-host failures are logged, not fatal — a domain
/// whose zone isn't delegated to Cloudflare simply isn't wired yet.
pub async fn ensure_domains(state: &AppState, rendered: &BTreeMap<String, String>) -> Result<()> {
    let Some(token) = state.config.cloudflare_token.clone() else {
        return Ok(());
    };
    let hosts = production_hosts(rendered);
    if hosts.is_empty() {
        return Ok(());
    }
    let prod_ip = prod_public_ip(state)
        .await
        .context("resolving prod public IP for Cloudflare DNS")?;
    let cf = Cloudflare::new(state.http.clone(), token);
    let mut zones: std::collections::BTreeMap<String, Zone> = Default::default();
    for host in hosts {
        match cf.zone_for(&host).await {
            Err(e) => tracing::warn!(
                host,
                error = format!("{e:#}"),
                "skipping (no Cloudflare zone)"
            ),
            Ok(zone) => {
                if let Err(e) = cf.ensure_dns_a(&zone, &host, &prod_ip).await {
                    tracing::error!(
                        host,
                        error = format!("{e:#}"),
                        "Cloudflare DNS ensure failed"
                    );
                } else if let Err(e) = cf.ensure_ssl_strict(&zone).await {
                    tracing::error!(
                        zone = zone.name,
                        error = format!("{e:#}"),
                        "Cloudflare SSL mode failed"
                    );
                } else {
                    tracing::info!(host, ip = prod_ip, "Cloudflare edge ensured");
                    zones.entry(zone.name.clone()).or_insert(zone);
                }
            }
        }
    }

    // Ensure an Origin CA cert exists (committed to git, key age-encrypted) for
    // each touched zone, so the reconciler can serve TLS on brand-new zones.
    if let Some(recipient) = state.config.age_production_recipient.clone() {
        for zone in zones.into_values() {
            if let Err(e) = ensure_origin_cert(state, &cf, &zone, &recipient).await {
                tracing::error!(
                    zone = zone.name,
                    error = format!("{e:#}"),
                    "origin cert ensure failed"
                );
            }
        }
    } else {
        tracing::debug!("MAJNET_AGE_PRODUCTION_RECIPIENT unset — skipping origin-cert issuance");
    }
    Ok(())
}

/// Issue + commit a Cloudflare Origin CA certificate for `zone` if one isn't
/// already in the platform repo. The cert lands plaintext, the private key
/// age-encrypted to the production recipient — only the reconciler decrypts it.
async fn ensure_origin_cert(
    state: &AppState,
    cf: &Cloudflare,
    zone: &Zone,
    recipient: &str,
) -> Result<()> {
    let org = &state.config.root_org;
    let client = state.github.org_client(org).await?;
    let crt_path = format!("platform/edge-main/certs/{}.crt", zone.name);
    let key_path = format!("platform/edge-main/certs/{}.key.age", zone.name);

    if crate::platform_api::read_platform_file(&client, org, &crt_path)
        .await
        .is_ok()
    {
        return Ok(()); // already issued
    }

    tracing::info!(zone = zone.name, "issuing Cloudflare Origin CA certificate");
    let cert = cf.issue_origin_cert(&zone.name).await?;
    let key_enc = age_encrypt(recipient, &cert.key_pem)
        .await
        .context("age-encrypting origin key")?;

    // Commit via the Contents API, not git-data: the App's git-data writes to
    // the platform repo 403 as "not accessible by integration" (unlike the ops
    // repo), while the Contents API works — it's the same path node enrollment
    // uses for nodes.yaml. Key first, then cert, so the reconciler (which keys
    // on the .crt) only acts once both files exist.
    put_platform_file(
        &client,
        org,
        &key_path,
        &key_enc,
        &format!("edge: origin key for {}", zone.name),
    )
    .await?;
    put_platform_file(
        &client,
        org,
        &crt_path,
        &cert.cert_pem,
        &format!("edge: origin cert for {}", zone.name),
    )
    .await?;
    state
        .store
        .log_event("origin-cert", Some(org), &zone.name)?;
    tracing::info!(zone = zone.name, "origin certificate committed");
    Ok(())
}

/// Create-or-update a file on the platform repo's `main` via the Contents API.
pub(crate) async fn put_platform_file(
    client: &octocrab::Octocrab,
    org: &str,
    path: &str,
    content: &str,
    message: &str,
) -> Result<()> {
    let repos = client.repos(org, "platform");
    match repos.get_content().path(path).r#ref("main").send().await {
        Ok(c) => {
            let sha = c
                .items
                .into_iter()
                .next()
                .context("empty contents response")?
                .sha;
            repos
                .update_file(path, message, content, &sha)
                .branch("main")
                .send()
                .await
                .with_context(|| format!("updating {path}"))?;
        }
        Err(octocrab::Error::GitHub { source, .. }) if source.status_code == 404 => {
            repos
                .create_file(path, message, content)
                .branch("main")
                .send()
                .await
                .with_context(|| format!("creating {path}"))?;
        }
        Err(e) => return Err(anyhow::Error::new(e).context(format!("reading {path}"))),
    }
    Ok(())
}

/// Encrypt `plaintext` to an age recipient (armored), via the `age` binary.
pub(crate) async fn age_encrypt(recipient: &str, plaintext: &str) -> Result<String> {
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new("age")
        .args(["-a", "-r", recipient])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("spawning age (is it installed?)")?;
    child
        .stdin
        .take()
        .context("no stdin")?
        .write_all(plaintext.as_bytes())
        .await?;
    let out = child.wait_with_output().await?;
    anyhow::ensure!(
        out.status.success(),
        "age encrypt failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(String::from_utf8(out.stdout)?)
}

/// Production hostnames declared by ingress across the rendered app manifests
/// (top-level `<app>.yaml`; skips `secrets/…`).
fn production_hosts(rendered: &BTreeMap<String, String>) -> Vec<String> {
    let mut hosts: Vec<String> = rendered
        .iter()
        .filter(|(path, _)| !path.contains('/') && path.ends_with(".yaml"))
        .filter_map(|(_, yaml)| AppManifest::parse(yaml).ok())
        .filter_map(|m| m.ingress)
        .flat_map(|ing| {
            ing.hosts()
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        })
        .collect();
    hosts.sort();
    hosts.dedup();
    hosts
}

/// The prod node's public IPv4, from the platform `nodes.yaml`.
async fn prod_public_ip(state: &AppState) -> Result<String> {
    let client = state.github.org_client(&state.config.root_org).await?;
    let yaml =
        crate::platform_api::read_platform_file(&client, &state.config.root_org, "nodes.yaml")
            .await?;
    let nodes = NodesFile::parse(yaml.as_bytes())?;
    let prod = nodes
        .by_role("prod")
        .context("no prod node in nodes.yaml")?;
    let ip = prod
        .public_endpoint
        .rsplit_once(':')
        .map(|(ip, _)| ip)
        .unwrap_or(&prod.public_endpoint);
    anyhow::ensure!(!ip.is_empty(), "prod node has no public endpoint yet");
    Ok(ip.to_string())
}

pub struct Cloudflare {
    http: reqwest::Client,
    token: String,
}

#[derive(Debug, Deserialize)]
struct Envelope<T> {
    success: bool,
    #[serde(default)]
    errors: Vec<ApiError>,
    result: Option<T>,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    code: i64,
    message: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Zone {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Deserialize)]
struct DnsRecord {
    id: String,
    content: String,
    #[serde(default)]
    proxied: bool,
}

impl Cloudflare {
    pub fn new(http: reqwest::Client, token: String) -> Self {
        Self { http, token }
    }

    /// The zone that owns `host` — the registrable zone whose name equals or is
    /// a dotted suffix of `host`, longest match first. Errors if the host's
    /// domain isn't delegated to this Cloudflare account (the one thing the
    /// bot can't fix — nameserver delegation is manual).
    pub async fn zone_for(&self, host: &str) -> Result<Zone> {
        let zones: Vec<Zone> = self
            .get("/zones?per_page=50&status=active")
            .await
            .context("listing Cloudflare zones")?;
        select_zone(host, &zones).cloned().with_context(|| {
            format!(
                "no Cloudflare zone for '{host}' — is the domain added to this account and its \
                 nameservers delegated to Cloudflare? (zone creation/delegation is manual)"
            )
        })
    }

    /// Ensure a **proxied** A record `name → ip` exists (create or update).
    pub async fn ensure_dns_a(&self, zone: &Zone, name: &str, ip: &str) -> Result<()> {
        let existing: Vec<DnsRecord> = self
            .get(&format!(
                "/zones/{}/dns_records?type=A&name={name}",
                zone.id
            ))
            .await
            .context("listing DNS records")?;
        let body = serde_json::json!({
            "type": "A", "name": name, "content": ip, "proxied": true, "ttl": 1
        });
        match existing.first() {
            Some(rec) if rec.content == ip && rec.proxied => Ok(()),
            Some(rec) => self
                .send(
                    reqwest::Method::PATCH,
                    &format!("/zones/{}/dns_records/{}", zone.id, rec.id),
                    Some(body),
                )
                .await
                .with_context(|| format!("updating DNS record for {name}")),
            None => self
                .send(
                    reqwest::Method::POST,
                    &format!("/zones/{}/dns_records", zone.id),
                    Some(body),
                )
                .await
                .with_context(|| format!("creating DNS record for {name}")),
        }
    }

    /// Set the zone's SSL/TLS mode to Full (strict).
    pub async fn ensure_ssl_strict(&self, zone: &Zone) -> Result<()> {
        self.send(
            reqwest::Method::PATCH,
            &format!("/zones/{}/settings/ssl", zone.id),
            Some(serde_json::json!({ "value": "strict" })),
        )
        .await
        .context("setting SSL mode to Full (strict)")
    }

    /// Issue a Cloudflare Origin CA certificate covering `zone` and `*.zone`.
    /// Generates an EC keypair + CSR locally (openssl); the private key is
    /// returned and never leaves the caller except age-encrypted into git.
    pub async fn issue_origin_cert(&self, zone: &str) -> Result<OriginCert> {
        let (key_pem, csr_pem) = generate_csr(zone).await?;
        #[derive(Deserialize)]
        struct CertResult {
            certificate: String,
        }
        let resp = self
            .http
            .post(format!("{API}/certificates"))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({
                "hostnames": [zone, format!("*.{zone}")],
                "requested_validity": 5475,
                "request_type": "origin-ecc",
                "csr": csr_pem,
            }))
            .send()
            .await?;
        let result: CertResult = unwrap_envelope(resp)
            .await
            .context("issuing Cloudflare Origin CA certificate")?;
        Ok(OriginCert {
            cert_pem: result.certificate,
            key_pem,
        })
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let resp = self
            .http
            .get(format!("{API}{path}"))
            .bearer_auth(&self.token)
            .send()
            .await?;
        unwrap_envelope(resp).await
    }

    /// Send a mutating request and discard the (unmodelled) result body.
    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<()> {
        let mut req = self
            .http
            .request(method, format!("{API}{path}"))
            .bearer_auth(&self.token);
        if let Some(body) = body {
            req = req.json(&body);
        }
        let _: serde_json::Value = unwrap_envelope(req.send().await?).await?;
        Ok(())
    }
}

async fn unwrap_envelope<T: serde::de::DeserializeOwned>(resp: reqwest::Response) -> Result<T> {
    let status = resp.status();
    let env: Envelope<T> = resp
        .json()
        .await
        .with_context(|| format!("decoding Cloudflare response (HTTP {status})"))?;
    if !env.success {
        let msg = env
            .errors
            .iter()
            .map(|e| format!("[{}] {}", e.code, e.message))
            .collect::<Vec<_>>()
            .join("; ");
        bail!("Cloudflare API error (HTTP {status}): {msg}");
    }
    env.result
        .context("Cloudflare response had success=true but no result")
}

/// A freshly issued origin certificate and its private key (both PEM).
pub struct OriginCert {
    pub cert_pem: String,
    pub key_pem: String,
}

/// Generate an EC (P-256) keypair + CSR for `zone` and `*.zone` via openssl.
/// Returns (private key PEM, CSR PEM).
async fn generate_csr(zone: &str) -> Result<(String, String)> {
    let dir = std::env::temp_dir().join(format!(
        "majnet-csr-{}-{}",
        zone.replace('.', "_"),
        std::process::id()
    ));
    tokio::fs::create_dir_all(&dir).await?;
    let key = dir.join("key.pem");
    let csr = dir.join("csr.pem");
    let out = tokio::process::Command::new("openssl")
        .args([
            "req",
            "-new",
            "-newkey",
            "ec",
            "-pkeyopt",
            "ec_paramgen_curve:prime256v1",
            "-nodes",
            "-keyout",
            key.to_str().context("non-utf8 temp path")?,
            "-out",
            csr.to_str().context("non-utf8 temp path")?,
            "-subj",
            &format!("/CN={zone}"),
            "-addext",
            &format!("subjectAltName=DNS:{zone},DNS:*.{zone}"),
        ])
        .output()
        .await
        .context("spawning openssl (is it installed?)")?;
    let result = async {
        anyhow::ensure!(
            out.status.success(),
            "openssl req failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        Ok::<_, anyhow::Error>((
            tokio::fs::read_to_string(&key).await?,
            tokio::fs::read_to_string(&csr).await?,
        ))
    }
    .await;
    let _ = tokio::fs::remove_dir_all(&dir).await;
    result
}

/// Longest-suffix zone match for a hostname. `app.majksa.cz` → `majksa.cz`.
fn select_zone<'a>(host: &str, zones: &'a [Zone]) -> Option<&'a Zone> {
    zones
        .iter()
        .filter(|z| host == z.name || host.ends_with(&format!(".{}", z.name)))
        .max_by_key(|z| z.name.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zones() -> Vec<Zone> {
        ["majksa.net", "majksa.cz", "sub.majksa.net"]
            .iter()
            .map(|n| Zone {
                id: format!("id-{n}"),
                name: n.to_string(),
            })
            .collect()
    }

    #[test]
    fn picks_longest_matching_zone() {
        let z = zones();
        assert_eq!(select_zone("app.majksa.cz", &z).unwrap().name, "majksa.cz");
        assert_eq!(select_zone("majksa.net", &z).unwrap().name, "majksa.net");
        // Longest suffix wins over the parent zone.
        assert_eq!(
            select_zone("a.sub.majksa.net", &z).unwrap().name,
            "sub.majksa.net"
        );
        assert!(select_zone("app.example.org", &z).is_none());
        // Not a real suffix (no dot boundary).
        assert!(select_zone("notmajksa.net", &z).is_none());
    }
}
