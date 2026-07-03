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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Database {
    pub engine: DbEngine,
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
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthCheck {
    pub path: String,
    pub port: u16,
    #[serde(default = "default_retries")]
    pub retries: u32,
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
        let Some((repo, digest)) = self.image.split_once('@') else {
            bail!(
                "image '{}' is not digest-pinned (expected repo@sha256:…)",
                self.image
            );
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
        if let Some(ingress) = &self.ingress {
            ensure!(
                !ingress.host.is_empty()
                    && ingress
                        .host
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || ".-".contains(c)),
                "ingress host '{}' is invalid",
                ingress.host
            );
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
        if let Some(migration) = &self.migration {
            ensure!(
                !migration.command.is_empty(),
                "migration command must not be empty"
            );
        }
        for secret in &self.secrets {
            ensure!(
                !secret.is_empty() && !secret.contains('/') && !secret.contains(".."),
                "secret name '{secret}' must be a bare file name"
            );
        }
        Ok(())
    }
}

/// One-shot migration container run before the blue-green rollout (§12).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Migration {
    pub command: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::AppManifest;

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
    fn rejects_bad_names_and_paths() {
        assert!(AppManifest::parse(&valid().replace("name: api", "name: API")).is_err());
        assert!(AppManifest::parse(&valid().replace("name: api", "name: -api")).is_err());
        let with_secret = format!("{}secrets: [../etc/passwd]\n", valid());
        assert!(AppManifest::parse(&with_secret).is_err());
    }
}
