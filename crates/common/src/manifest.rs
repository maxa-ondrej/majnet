//! Manifest schema v1 — per-app `base.yaml` merged with a thin class overlay.
//!
//! Rendering (base ⊕ overlay) is done by the bot; the reconciler consumes only
//! the final manifests from the `env/<class>` branches and re-validates
//! defensively. Secrets pass through SOPS-encrypted — rendering never decrypts.

use anyhow::{bail, ensure, Result};
use serde::{Deserialize, Serialize};

/// A rendered application manifest as it appears on an `env/<class>` branch.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppManifest {
    pub name: String,
    /// Container image. Either a full pinned reference (`ghcr.io/<org>/<app>@sha256:…`)
    /// — the original single-field form — or a bare repository (`ghcr.io/<org>/<app>`)
    /// paired with a `digest` (or `tag`). The bare-repo form lets `base.yaml` carry
    /// the env-unspecific repository while each class overlay carries its own
    /// env-specific pin. Resolve the effective reference with [`Self::image_ref`].
    pub image: String,
    /// Env-specific digest pin (`sha256:…`) for a bare-repo `image`. Skipped when
    /// empty so existing combined-`image` manifests serialize identically (no
    /// `config_hash` change / fleet recycle).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    /// Env-specific tag for a bare-repo `image` (used only when `digest` is unset;
    /// note §5 requires digest-pinning, so a tag alone fails validation — kept for
    /// forward flexibility). Skipped when empty (byte-compat).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    /// Secrets delivered to the container as tmpfs files at `/run/secrets/<KEY>`
    /// (never env vars). Two on-disk shapes during the SOPS→inline migration
    /// (ADR 0024) — see [`Secrets`]. Always serialized (default = empty legacy
    /// list → `secrets: []`, exactly as the pre-0024 `Vec<String>` field did), so
    /// existing apps' serialized manifests — and thus their `config_hash` — are
    /// byte-identical: the schema change triggers no fleet recycle.
    #[serde(default)]
    pub secrets: Secrets,
    #[serde(default)]
    pub ingress: Option<Ingress>,
    #[serde(default)]
    pub health: Option<HealthCheck>,
    #[serde(default)]
    pub migration: Option<Migration>,
    /// Managed database (§15): the reconciler provisions a logical DB + user
    /// on the class-appropriate engine instance and injects connection env.
    #[serde(default)]
    pub database: Option<Database>,
    /// Optional container resource limits (memory, CPU). Absent = unlimited
    /// (Docker default). Applied to the app container's HostConfig at deploy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<Resources>,
    /// Persistent named volumes mounted into the container. Each is backed by a
    /// Docker named volume on the app's node, survives redeploys (blue-green
    /// reuses it), and is never deleted on teardown — data is preserved
    /// ("archive, never delete"). For stateful apps that write to disk.
    #[serde(default)]
    pub volumes: Vec<Volume>,
    /// Number of container replicas to run, round-robin load-balanced by the
    /// edge Traefik. Defaults to 1. Capped at 1 for volume-backed apps
    /// (a persistent volume is single-writer).
    #[serde(default = "default_replicas")]
    pub replicas: u32,
    /// Opt into OpenTelemetry (ADR 0023): when set, and the platform has an OTLP
    /// collector endpoint configured, the reconciler injects
    /// `OTEL_EXPORTER_OTLP_ENDPOINT` + `OTEL_RESOURCE_ATTRIBUTES` (service.name,
    /// deployment.environment, project) into the container — the app just needs
    /// an OTEL SDK. Inert until a collector exists, so it's safe to set ahead of
    /// the backend. Per-manifest (per-class via overlays).
    #[serde(default)]
    pub otel: bool,
    /// Container ports to publish on the node's WireGuard mesh IP so they are
    /// reachable fleet-wide over the WG tunnel — not just within the project
    /// network or from same-node containers. Each listed container port is bound
    /// to `<node wireguard_ip>:<same port>`; never to a public interface.
    ///
    /// Used for cross-node service endpoints: e.g. the OTEL collector on the
    /// private node publishing `[4317, 4318]` so prod-node apps can push OTLP
    /// over WG (ADR 0023). Empty (the default) publishes nothing. Requires
    /// `replicas: 1` — a fixed host port has a single binder.
    ///
    /// Skipped when empty on serialization so adding this field doesn't perturb
    /// the reconciler's `config_hash` for existing apps — no fleet-wide recycle
    /// on rollout; only an app that actually sets `wg_ports` re-converges.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wg_ports: Vec<u16>,
}

/// An app's secrets. Two on-disk shapes during the SOPS→inline migration
/// (ADR 0024), distinguished by YAML shape (an app/class uses one or the other):
///
/// - [`Secrets::Inline`] — a `KEY: majnet:<base64(age ciphertext)>` map. The
///   value is encrypted in place with the platform class recipient; only the
///   reconciler decrypts it, at deploy time. This is the target model.
/// - [`Secrets::Names`] — a bare allowlist of names whose values live in the
///   legacy `secrets.<class>.yaml` SOPS file (pre-migration).
///
/// The `majnet:` envelope keeps each value on a single line. Empty (the default)
/// declares no secrets. Rendering never decrypts either shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Secrets {
    /// New: `KEY → majnet:<ciphertext>` inline map.
    Inline(std::collections::BTreeMap<String, String>),
    /// Legacy: bare-name allowlist; values in the sibling SOPS file.
    Names(Vec<String>),
}

/// The single-line encrypted-value prefix (`majnet:<base64(age ciphertext)>`).
pub const SECRET_ENVELOPE_PREFIX: &str = "majnet:";

impl Default for Secrets {
    /// Empty legacy list, so a manifest with no secrets serializes as `secrets: []`
    /// — byte-identical to the pre-ADR-0024 `Vec<String>` default (no config_hash
    /// change / fleet recycle when this schema lands).
    fn default() -> Self {
        Secrets::Names(Vec::new())
    }
}

impl Secrets {
    pub fn is_empty(&self) -> bool {
        match self {
            Secrets::Inline(m) => m.is_empty(),
            Secrets::Names(v) => v.is_empty(),
        }
    }

    /// The inline `KEY → majnet:ciphertext` map, if declared inline and non-empty.
    pub fn inline(&self) -> Option<&std::collections::BTreeMap<String, String>> {
        match self {
            Secrets::Inline(m) if !m.is_empty() => Some(m),
            _ => None,
        }
    }

    /// The legacy bare-name allowlist, if declared as a list and non-empty.
    pub fn names(&self) -> Option<&[String]> {
        match self {
            Secrets::Names(v) if !v.is_empty() => Some(v.as_slice()),
            _ => None,
        }
    }
}

fn default_replicas() -> u32 {
    1
}

/// A persistent named volume mounted into the app container. `name` identifies
/// it within the app (→ the Docker volume `majnet-<project>-<app>-<class>-<name>`);
/// `path` is the absolute in-container mount point.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Volume {
    pub name: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Database {
    pub engine: DbEngine,
}

/// Optional container resource limits (→ bollard `HostConfig`). An absent field
/// means unlimited (the Docker default). Memory takes Docker-style suffixes
/// (`b`/`k`/`m`/`g`, base 1024); cpus is a core count (`0.5`, `2`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpus: Option<String>,
}

impl Resources {
    /// Hard memory limit in bytes, if set.
    pub fn memory_bytes(&self) -> Result<Option<i64>> {
        self.memory.as_deref().map(parse_memory).transpose()
    }
    /// CPU limit in nano-CPUs (cores × 1e9), if set.
    pub fn nano_cpus(&self) -> Result<Option<i64>> {
        self.cpus.as_deref().map(parse_cpus).transpose()
    }
}

/// Parse a Docker-style memory string (`512m`, `2g`, `1073741824`) → bytes.
pub fn parse_memory(s: &str) -> Result<i64> {
    let s = s.trim();
    ensure!(!s.is_empty(), "memory limit is empty");
    let (num, mult) = match s.chars().last().unwrap() {
        'b' | 'B' => (&s[..s.len() - 1], 1i64),
        'k' | 'K' => (&s[..s.len() - 1], 1024),
        'm' | 'M' => (&s[..s.len() - 1], 1024 * 1024),
        'g' | 'G' => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        c if c.is_ascii_digit() => (s, 1),
        other => bail!("invalid memory suffix '{other}' in '{s}' (use b/k/m/g)"),
    };
    let n: f64 = num
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid memory value '{s}'"))?;
    ensure!(n > 0.0, "memory limit must be positive");
    Ok((n * mult as f64) as i64)
}

/// Parse a CPU core count (`0.5`, `2`) → nano-CPUs.
pub fn parse_cpus(s: &str) -> Result<i64> {
    let n: f64 = s
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid cpus value '{s}'"))?;
    ensure!(n > 0.0, "cpus must be positive");
    Ok((n * 1e9) as i64)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DbEngine {
    Postgres,
    Mariadb,
    Valkey,
    Mongodb,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ingress {
    /// Primary public hostname. Optional (ADR 0013): non-production classes get
    /// an auto-assigned `{app}.{project}.{base_domain}` at render time, so the
    /// app declares only `port`. For `production` this is the app's real custom
    /// domain and drives Cloudflare + edge routing (ADR 0007).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Container port the ingress forwards to.
    pub port: u16,
    /// Additional public hostnames, possibly across several Cloudflare zones
    /// (ADR 0007). The full set the router serves is `[host] + domains`.
    #[serde(default)]
    pub domains: Vec<String>,
    /// Expose this app to the public internet via a Cloudflare Tunnel (ADR 0026).
    /// Only meaningful for non-production classes (production is already public via
    /// `edge-main`): the reconciler runs a `cloudflared` sidecar on the private node
    /// that dials out to Cloudflare, so the app's `host` is reachable publicly
    /// without opening any inbound port. Requires a `host`. Skipped when false so
    /// existing manifests serialize byte-identically (no `config_hash` change).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub public: bool,
}

impl Ingress {
    /// Every public hostname this ingress serves — primary first, if set.
    pub fn hosts(&self) -> Vec<&str> {
        self.host
            .as_deref()
            .into_iter()
            .chain(self.domains.iter().map(String::as_str))
            .collect()
    }
}

/// Whether an image string already carries a pin — a `@digest` or a `:tag` on
/// its last path segment (a `:` in a registry host like `host:5000/repo` is not
/// a tag). A bare repo (no pin) draws its pin from the `digest`/`tag` fields.
pub fn image_has_pin(image: &str) -> bool {
    image.contains('@') || image.rsplit('/').next().unwrap_or(image).contains(':')
}

/// The full image reference to pull/deploy: the `image` field if it already
/// carries a pin, otherwise the bare repo joined with the split `digest` (or
/// `tag`) field. This is the single source of truth every consumer uses so the
/// combined and split forms are interchangeable.
impl AppManifest {
    pub fn image_ref(&self) -> String {
        if image_has_pin(&self.image) {
            self.image.clone()
        } else if let Some(d) = &self.digest {
            format!("{}@{}", self.image, d)
        } else if let Some(t) = &self.tag {
            format!("{}:{}", self.image, t)
        } else {
            self.image.clone() // unpinned — rejected by validate()
        }
    }
}

/// Enforce `repo@sha256:<64 hex>` — images are pinned by digest, never by tag
/// (§5).
pub fn validate_digest_pinned(image: &str) -> Result<()> {
    let Some((repo, digest)) = image.split_once('@') else {
        bail!("image '{image}' is not digest-pinned (expected repo@sha256:…)");
    };
    ensure!(
        !repo.is_empty() && !repo.contains(' '),
        "image repository '{repo}' is invalid"
    );
    ensure!(
        digest
            .strip_prefix("sha256:")
            .is_some_and(|h| h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit())),
        "image digest '{digest}' is not a valid sha256 digest"
    );
    Ok(())
}

fn is_valid_hostname(h: &str) -> bool {
    // A DNS name (labels of alphanumerics/hyphens). FQDN-ness is not required
    // here — stable/ephemeral tailnet ingress names can be single-label;
    // production domains are checked for a real zone by the bot (ADR 0007).
    !h.is_empty()
        && h.len() <= 253
        && h.chars()
            .all(|c| c.is_ascii_alphanumeric() || ".-".contains(c))
        && !h.starts_with(['.', '-'])
        && !h.ends_with(['.', '-'])
        && !h.contains("..")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthCheck {
    /// HTTP path probed for liveness. Defaults to the platform-standard
    /// `/healthz` so an app only needs to declare its `port` to opt in — the
    /// same path the reconciler scrapes `/info` alongside.
    #[serde(default = "default_health_path")]
    pub path: String,
    pub port: u16,
    #[serde(default = "default_retries")]
    pub retries: u32,
}

/// The platform-standard liveness path (§ "standard endpoints"). Apps are
/// encouraged to serve `/healthz` (liveness) and `/info` (build metadata) on
/// their HTTP port; the reconciler health-gates on the former and records the
/// latter per app/env.
fn default_health_path() -> String {
    "/healthz".into()
}

fn default_retries() -> u32 {
    5
}

impl AppManifest {
    /// Parse + strictly validate a rendered manifest. Run by the bot before
    /// committing to an env branch and again, defensively, by the reconciler
    /// before deploying (§12.2). Failure aborts that app loudly — no partial
    /// applies.
    pub fn parse(yaml: &str) -> Result<Self> {
        let manifest: Self = serde_yaml::from_str(yaml)?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.name.is_empty()
                && self.name.len() <= 63
                && self
                    .name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                && !self.name.starts_with('-')
                && !self.name.ends_with('-'),
            "app name '{}' must be lowercase alphanumeric/hyphens (DNS label)",
            self.name
        );
        // Images are pinned by digest, never by tag (§5 decision log). Validated
        // on the *effective* reference so the combined (`image: repo@digest`) and
        // split (`image: repo` + `digest:`) forms are held to the same rule.
        validate_digest_pinned(&self.image_ref())?;
        if let Some(ingress) = &self.ingress {
            for host in ingress.hosts() {
                ensure!(
                    is_valid_hostname(host),
                    "ingress hostname '{host}' is not a valid DNS name"
                );
            }
            ensure!(ingress.port != 0, "ingress port must be non-zero");
            ensure!(
                !ingress.public || ingress.host.is_some(),
                "ingress.public requires a `host` (the public hostname to route via the tunnel)"
            );
        }
        if let Some(health) = &self.health {
            ensure!(
                health.path.starts_with('/'),
                "health path '{}' must start with /",
                health.path
            );
            ensure!(health.port != 0, "health port must be non-zero");
        }
        if let Some(res) = &self.resources {
            // Surface a bad value at render/validate time, not at deploy.
            res.memory_bytes()?;
            res.nano_cpus()?;
        }
        if let Some(migration) = &self.migration {
            ensure!(
                !migration.command.is_empty(),
                "migration command must not be empty"
            );
            if let Some(image) = &migration.image {
                validate_digest_pinned(image)?;
            }
        }
        match &self.secrets {
            // Legacy allowlist: bare file names (values in the SOPS file).
            Secrets::Names(names) => {
                for secret in names {
                    ensure!(
                        !secret.is_empty() && !secret.contains('/') && !secret.contains(".."),
                        "secret name '{secret}' must be a bare file name"
                    );
                }
            }
            // Inline (ADR 0024): each key becomes a `/run/secrets/<KEY>` file, so
            // it must be a bare name; each value is a `majnet:` encrypted blob.
            Secrets::Inline(map) => {
                for (key, val) in map {
                    ensure!(
                        !key.is_empty() && !key.contains('/') && !key.contains(".."),
                        "secret key '{key}' must be a bare name"
                    );
                    let body = val.strip_prefix(SECRET_ENVELOPE_PREFIX).ok_or_else(|| {
                        anyhow::anyhow!(
                            "secret '{key}' must be a '{SECRET_ENVELOPE_PREFIX}…' encrypted blob"
                        )
                    })?;
                    ensure!(
                        !body.is_empty()
                            && body.bytes().all(|b| b.is_ascii_alphanumeric()
                                || b == b'+'
                                || b == b'/'
                                || b == b'='),
                        "secret '{key}' has a malformed '{SECRET_ENVELOPE_PREFIX}' envelope"
                    );
                }
            }
        }
        let mut seen_names = std::collections::BTreeSet::new();
        let mut seen_paths = std::collections::BTreeSet::new();
        for v in &self.volumes {
            ensure!(
                !v.name.is_empty()
                    && v.name.len() <= 63
                    && v.name
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                    && !v.name.starts_with('-')
                    && !v.name.ends_with('-'),
                "volume name '{}' must be lowercase alphanumeric/hyphens (DNS label)",
                v.name
            );
            ensure!(
                v.path.starts_with('/') && !v.path.contains(".."),
                "volume path '{}' must be an absolute path",
                v.path
            );
            ensure!(
                seen_names.insert(v.name.as_str()),
                "duplicate volume name '{}'",
                v.name
            );
            ensure!(
                seen_paths.insert(v.path.as_str()),
                "duplicate volume mount path '{}'",
                v.path
            );
        }
        ensure!(self.replicas >= 1, "replicas must be at least 1");
        ensure!(
            self.replicas == 1 || self.volumes.is_empty(),
            "cannot run more than one replica of an app with persistent volumes (single-writer)"
        );
        for p in &self.wg_ports {
            ensure!(*p != 0, "wg_ports entry must be a non-zero port");
        }
        ensure!(
            self.wg_ports.is_empty() || self.replicas == 1,
            "cannot run more than one replica of an app that publishes wg_ports (single host-port binder)"
        );
        Ok(())
    }
}

/// One-shot migration container run before the blue-green rollout (§12).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Migration {
    /// Migration image (digest-pinned); omitted = run `command` in the app image
    /// (ADR 0009). SQL/tooling migrations point this at a runner image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    pub command: Vec<String>,
}

impl Migration {
    /// The image the migration runs in — its own if set, else the app image.
    pub fn image<'a>(&'a self, app_image: &'a str) -> &'a str {
        self.image.as_deref().unwrap_or(app_image)
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_cpus, parse_memory, AppManifest};

    const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn valid() -> String {
        format!("name: api\nimage: ghcr.io/org/api@{DIGEST}\ningress:\n  host: api.proj.majksa.net\n  port: 8080\n")
    }

    #[test]
    fn accepts_valid_manifest() {
        assert!(AppManifest::parse(&valid()).is_ok());
    }

    #[test]
    fn rejects_tag_pinned_image() {
        let yaml = "name: api\nimage: ghcr.io/org/api:latest\n";
        assert!(AppManifest::parse(yaml)
            .unwrap_err()
            .to_string()
            .contains("digest-pinned"));
    }

    #[test]
    fn rejects_bad_digest() {
        let yaml = "name: api\nimage: ghcr.io/org/api@sha256:short\n";
        assert!(AppManifest::parse(yaml).is_err());
    }

    #[test]
    fn rejects_unknown_fields() {
        let yaml = format!("name: api\nimage: r@{DIGEST}\nreplica: 2\n");
        assert!(AppManifest::parse(&yaml).is_err());
    }

    #[test]
    fn parses_and_orders_ingress_domains() {
        let yaml = format!(
            "name: api\nimage: ghcr.io/org/api@{DIGEST}\ningress:\n  host: app.majksa.cz\n  port: 8080\n  domains:\n    - www.majksa.cz\n    - app.majksa.net\n"
        );
        let m = AppManifest::parse(&yaml).unwrap();
        let ingress = m.ingress.unwrap();
        assert_eq!(
            ingress.hosts(),
            vec!["app.majksa.cz", "www.majksa.cz", "app.majksa.net"]
        );
    }

    #[test]
    fn ingress_public_requires_host() {
        let no_host = format!(
            "name: api\nimage: ghcr.io/org/api@{DIGEST}\ningress:\n  port: 8080\n  public: true\n"
        );
        assert!(AppManifest::parse(&no_host)
            .unwrap_err()
            .to_string()
            .contains("requires a `host`"));
        // With a host it parses fine.
        let with_host = format!("name: api\nimage: ghcr.io/org/api@{DIGEST}\ningress:\n  host: dev.example.com\n  port: 8080\n  public: true\n");
        assert!(
            AppManifest::parse(&with_host)
                .unwrap()
                .ingress
                .unwrap()
                .public
        );
    }

    #[test]
    fn ingress_public_false_is_not_serialized() {
        // Byte-compat: an ingress that never set `public` must not emit `public:`
        // so existing manifests' config_hash is unchanged (no fleet recycle).
        let m = AppManifest::parse(&valid()).unwrap();
        let out = serde_yaml::to_string(&m).unwrap();
        assert!(!out.contains("public"), "unexpected `public` in:\n{out}");
    }

    #[test]
    fn image_ref_handles_combined_and_split_forms() {
        // Combined form: passed through unchanged.
        let m = AppManifest::parse(&format!("name: api\nimage: ghcr.io/o/api@{DIGEST}\n")).unwrap();
        assert_eq!(m.image_ref(), format!("ghcr.io/o/api@{DIGEST}"));
        assert!(m.digest.is_none());
        // Split form: bare repo + digest reconstructs to the pinned reference.
        let s = AppManifest::parse(&format!(
            "name: api\nimage: ghcr.io/o/api\ndigest: {DIGEST}\n"
        ))
        .unwrap();
        assert_eq!(s.image_ref(), format!("ghcr.io/o/api@{DIGEST}"));
        // A registry port in the repo is not mistaken for a tag.
        assert!(!super::image_has_pin("registry.example.com:5000/o/api"));
        assert!(super::image_has_pin("ghcr.io/o/api:v1"));
        // A tag-only split still fails §5 digest-pinning.
        assert!(AppManifest::parse("name: api\nimage: ghcr.io/o/api\ntag: v1\n").is_err());
        // Byte-compat: a combined manifest never serializes digest/tag keys.
        assert!(!serde_yaml::to_string(&m).unwrap().contains("digest:"));
    }

    #[test]
    fn parses_volumes() {
        let yaml = format!(
            "name: api\nimage: ghcr.io/org/api@{DIGEST}\nvolumes:\n  - name: data\n    path: /app/data\n"
        );
        let m = AppManifest::parse(&yaml).unwrap();
        assert_eq!(m.volumes.len(), 1);
        assert_eq!(m.volumes[0].name, "data");
        assert_eq!(m.volumes[0].path, "/app/data");
    }

    #[test]
    fn health_path_defaults_to_healthz() {
        // A health block with only a port gets the standard `/healthz` path.
        let yaml = format!("name: api\nimage: ghcr.io/org/api@{DIGEST}\nhealth:\n  port: 8080\n");
        let m = AppManifest::parse(&yaml).unwrap();
        let health = m.health.unwrap();
        assert_eq!(health.path, "/healthz");
        assert_eq!(health.port, 8080);
    }

    #[test]
    fn replicas_default_is_one() {
        let m =
            AppManifest::parse(&format!("name: api\nimage: ghcr.io/org/api@{DIGEST}\n")).unwrap();
        assert_eq!(m.replicas, 1);
    }

    #[test]
    fn rejects_replicas_with_volume() {
        let yaml = format!(
            "name: api\nimage: ghcr.io/org/api@{DIGEST}\nreplicas: 3\nvolumes:\n  - name: data\n    path: /app/data\n"
        );
        assert!(AppManifest::parse(&yaml)
            .unwrap_err()
            .to_string()
            .contains("more than one replica"));
    }

    #[test]
    fn rejects_relative_volume_path() {
        let yaml = format!(
            "name: api\nimage: ghcr.io/org/api@{DIGEST}\nvolumes:\n  - name: data\n    path: app/data\n"
        );
        assert!(AppManifest::parse(&yaml)
            .unwrap_err()
            .to_string()
            .contains("absolute path"));
    }

    #[test]
    fn rejects_duplicate_volume_path() {
        let yaml = format!(
            "name: api\nimage: ghcr.io/org/api@{DIGEST}\nvolumes:\n  - name: a\n    path: /d\n  - name: b\n    path: /d\n"
        );
        assert!(AppManifest::parse(&yaml).is_err());
    }

    #[test]
    fn rejects_invalid_domain() {
        let yaml = format!(
            "name: api\nimage: ghcr.io/org/api@{DIGEST}\ningress:\n  host: app.majksa.cz\n  port: 8080\n  domains:\n    - 'bad host'\n"
        );
        assert!(AppManifest::parse(&yaml).is_err());
    }

    #[test]
    fn rejects_bad_names_and_paths() {
        assert!(AppManifest::parse(&valid().replace("name: api", "name: API")).is_err());
        assert!(AppManifest::parse(&valid().replace("name: api", "name: -api")).is_err());
        let with_secret = format!("{}secrets: [../etc/passwd]\n", valid());
        assert!(AppManifest::parse(&with_secret).is_err());
    }

    #[test]
    fn parses_legacy_name_list_secrets() {
        // Pre-ADR-0024 shape: a bare allowlist (values live in the SOPS file).
        let m =
            AppManifest::parse(&format!("{}secrets: [DATABASE_URL, API_KEY]\n", valid())).unwrap();
        assert_eq!(
            m.secrets.names(),
            Some(&["DATABASE_URL".into(), "API_KEY".into()][..])
        );
        assert!(m.secrets.inline().is_none());
    }

    #[test]
    fn parses_inline_encrypted_secrets() {
        let yaml = format!(
            "{}secrets:\n  DATABASE_URL: majnet:AgV1aGVsbG8=\n  API_KEY: majnet:AgB9d29ybGQ=\n",
            valid()
        );
        let m = AppManifest::parse(&yaml).unwrap();
        let inline = m.secrets.inline().expect("inline map");
        assert_eq!(
            inline.get("DATABASE_URL").map(String::as_str),
            Some("majnet:AgV1aGVsbG8=")
        );
        assert!(m.secrets.names().is_none());
    }

    #[test]
    fn rejects_inline_secret_without_envelope() {
        // A plaintext value (no `majnet:` prefix) must be refused.
        let yaml = format!(
            "{}secrets:\n  DATABASE_URL: postgres://plaintext\n",
            valid()
        );
        assert!(AppManifest::parse(&yaml).is_err());
        // A bad base64 body is refused too.
        let bad = format!("{}secrets:\n  API_KEY: majnet:not base64!\n", valid());
        assert!(AppManifest::parse(&bad).is_err());
    }

    #[test]
    fn empty_secrets_serializes_as_legacy_empty_list() {
        // Byte-identical to the pre-ADR-0024 `Vec<String>` default (`secrets: []`),
        // so the reconciler's manifest-serialized config_hash is unchanged for
        // existing apps — the schema change triggers no fleet recycle.
        let m = AppManifest::parse(&valid()).unwrap();
        assert!(m.secrets.is_empty());
        assert!(serde_yaml::to_string(&m).unwrap().contains("secrets: []"));
    }

    #[test]
    fn inline_secrets_round_trip_as_a_map() {
        let yaml = format!("{}secrets:\n  API_KEY: majnet:AgB9d29ybGQ=\n", valid());
        let m = AppManifest::parse(&yaml).unwrap();
        let out = serde_yaml::to_string(&m).unwrap();
        assert!(out.contains("API_KEY: majnet:AgB9d29ybGQ="));
        // Re-parses to the same inline shape.
        assert!(AppManifest::parse(&out).unwrap().secrets.inline().is_some());
    }

    #[test]
    fn parses_wg_ports() {
        let yaml =
            format!("name: api\nimage: ghcr.io/org/api@{DIGEST}\nwg_ports:\n  - 4317\n  - 4318\n");
        let m = AppManifest::parse(&yaml).unwrap();
        assert_eq!(m.wg_ports, vec![4317, 4318]);
    }

    #[test]
    fn empty_wg_ports_is_not_serialized() {
        // Skipped-when-empty so the reconciler's manifest-serialized config_hash
        // is unchanged for existing apps — no fleet-wide recycle on rollout.
        let m = AppManifest::parse(&valid()).unwrap();
        assert!(m.wg_ports.is_empty());
        assert!(!serde_yaml::to_string(&m).unwrap().contains("wg_ports"));
    }

    #[test]
    fn rejects_wg_ports_with_replicas() {
        let yaml = format!(
            "name: api\nimage: ghcr.io/org/api@{DIGEST}\nreplicas: 2\nwg_ports:\n  - 4317\n"
        );
        assert!(AppManifest::parse(&yaml)
            .unwrap_err()
            .to_string()
            .contains("wg_ports"));
    }

    #[test]
    fn parses_resource_limits() {
        assert_eq!(parse_memory("512m").unwrap(), 512 * 1024 * 1024);
        assert_eq!(parse_memory("2G").unwrap(), 2 * 1024 * 1024 * 1024);
        assert_eq!(parse_memory("1048576").unwrap(), 1_048_576);
        assert_eq!(parse_cpus("0.5").unwrap(), 500_000_000);
        assert_eq!(parse_cpus("2").unwrap(), 2_000_000_000);
        assert!(parse_memory("lots").is_err());
        assert!(parse_memory("-1g").is_err());
        assert!(parse_cpus("fast").is_err());
        // A manifest with a bad resources value fails validation.
        let bad = format!("{}resources:\n  memory: huge\n", valid());
        assert!(AppManifest::parse(&bad).is_err());
        let ok = format!("{}resources:\n  memory: 256m\n  cpus: \"0.5\"\n", valid());
        let m = AppManifest::parse(&ok).unwrap();
        assert_eq!(
            m.resources.unwrap().memory_bytes().unwrap(),
            Some(256 * 1024 * 1024)
        );
    }
}
