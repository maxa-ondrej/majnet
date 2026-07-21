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
    /// Image pinned by digest (`ghcr.io/<org>/<app>@sha256:...`). Tags are not allowed.
    pub image: String,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub secrets: Vec<String>,
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
        // Images are pinned by digest, never by tag (§5 decision log).
        validate_digest_pinned(&self.image)?;
        if let Some(ingress) = &self.ingress {
            for host in ingress.hosts() {
                ensure!(
                    is_valid_hostname(host),
                    "ingress hostname '{host}' is not a valid DNS name"
                );
            }
            ensure!(ingress.port != 0, "ingress port must be non-zero");
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
        for secret in &self.secrets {
            ensure!(
                !secret.is_empty() && !secret.contains('/') && !secret.contains(".."),
                "secret name '{secret}' must be a bare file name"
            );
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
