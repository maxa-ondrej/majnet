//! Release descriptor (`majnet-release.yaml`) — the DEV→OPS contract (ADR 0009).
//!
//! An app repo's CI publishes this on a tag `vX.Y.Z`: an immutable, versioned
//! bundle of digest-pinned artifacts. The bot reads it off the `release`
//! webhook, records it, and (later) promotes it into the `ops` manifests.

use crate::manifest::validate_digest_pinned;
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Release {
    /// Human version, typically the git tag (`v1.4.2`).
    pub version: String,
    /// Source commit the artifacts were built from.
    pub commit: String,
    /// App runtime image, digest-pinned (`ghcr.io/org/app@sha256:…`).
    pub app: String,
    /// Optional migration run before the blue-green rollout (§12).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migration: Option<ReleaseMigration>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReleaseMigration {
    /// Migration image (digest-pinned). Omitted = run in the app image, so a
    /// simple app just gives a `command`; SQL/tooling migrations point this at a
    /// runner image (dbmate/flyway/…).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// The migration command (argv).
    pub command: Vec<String>,
}

impl Release {
    pub fn parse(yaml: &[u8]) -> Result<Self> {
        let r: Release = serde_yaml::from_slice(yaml)?;
        r.validate()?;
        Ok(r)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(!self.version.is_empty(), "release version is empty");
        ensure!(!self.commit.is_empty(), "release commit is empty");
        validate_digest_pinned(&self.app)?;
        if let Some(m) = &self.migration {
            ensure!(!m.command.is_empty(), "migration.command is empty");
            if let Some(image) = &m.image {
                validate_digest_pinned(image)?;
            }
        }
        Ok(())
    }

    /// The migration to run, resolved: (image, command). The image defaults to
    /// the app image when the descriptor omits it.
    pub fn resolved_migration(&self) -> Option<(&str, &[String])> {
        self.migration.as_ref().map(|m| {
            (
                m.image.as_deref().unwrap_or(&self.app),
                m.command.as_slice(),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::Release;

    const APP: &str =
        "ghcr.io/acme/blog@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const MIG: &str = "ghcr.io/acme/blog-migrate@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn parses_minimal_release() {
        let r = Release::parse(format!("version: v1.0.0\ncommit: abc123\napp: {APP}\n").as_bytes())
            .unwrap();
        assert_eq!(r.version, "v1.0.0");
        assert!(r.resolved_migration().is_none());
    }

    #[test]
    fn migration_defaults_to_app_image() {
        let y =
            format!("version: v1\ncommit: c\napp: {APP}\nmigration:\n  command: [migrate, up]\n");
        let r = Release::parse(y.as_bytes()).unwrap();
        let (image, cmd) = r.resolved_migration().unwrap();
        assert_eq!(image, APP);
        assert_eq!(cmd, ["migrate", "up"]);
    }

    #[test]
    fn separate_migration_image() {
        let y = format!("version: v1\ncommit: c\napp: {APP}\nmigration:\n  image: {MIG}\n  command: [dbmate, up]\n");
        let r = Release::parse(y.as_bytes()).unwrap();
        assert_eq!(r.resolved_migration().unwrap().0, MIG);
    }

    #[test]
    fn rejects_tag_pinned_app() {
        assert!(
            Release::parse(b"version: v1\ncommit: c\napp: ghcr.io/acme/blog:latest\n").is_err()
        );
    }

    #[test]
    fn rejects_empty_migration_command() {
        let y = format!("version: v1\ncommit: c\napp: {APP}\nmigration:\n  command: []\n");
        assert!(Release::parse(y.as_bytes()).is_err());
    }
}
