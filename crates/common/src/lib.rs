//! majnet-common — shared types for the MajNet v2 control plane.
//!
//! Home of the manifest schema (app `base.yaml` + class overlays), the
//! project config (`project.yaml`), the platform config (`nodes.yaml`,
//! `people.yaml`, `projects.yaml`) and strict validation used by both the
//! bot (at render time) and the reconciler (defensively at deploy time).

pub mod authz;
pub mod manifest;
pub mod merge;
pub mod platform;
pub mod project;
pub mod release;
pub mod tarball;

/// Environment classes — see design doc §8.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EnvClass {
    /// Public, gated behind a reviewed `env/production` render PR. Runs on the prod node.
    Production,
    /// VPN-only, deployed from a tagged release (`vX.Y.Z`), auto-merged. Runs on
    /// the private node (ADR 0009 — was auto-on-merge-to-main).
    Stable,
    /// VPN-only, continuous latest-`main` build, auto-merged. Runs on the
    /// private node (ADR 0009).
    Testing,
    /// VPN-only, PR-scoped preview. 48 h grace after PR close, 7 d hard TTL.
    Ephemeral,
}

impl EnvClass {
    pub const ALL: [EnvClass; 4] = [
        EnvClass::Production,
        EnvClass::Stable,
        EnvClass::Testing,
        EnvClass::Ephemeral,
    ];

    /// Static trust-zoned placement: the node follows from the class (§3, §4).
    pub fn node_role(self) -> &'static str {
        match self {
            EnvClass::Production => "prod",
            EnvClass::Stable | EnvClass::Testing | EnvClass::Ephemeral => "private",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            EnvClass::Production => "production",
            EnvClass::Stable => "stable",
            EnvClass::Testing => "testing",
            EnvClass::Ephemeral => "ephemeral",
        }
    }

    /// The rendered branch this class deploys from (§9).
    pub fn env_branch(self) -> String {
        format!("env/{}", self.as_str())
    }

    /// Render PRs for testing/stable/ephemeral auto-merge; production waits for
    /// an admin review of the final diff — that review IS the production gate.
    pub fn auto_merges(self) -> bool {
        !matches!(self, EnvClass::Production)
    }
}

#[cfg(test)]
mod tests {
    use super::EnvClass;

    #[test]
    fn class_gradient_placement_and_gating() {
        // Static placement (ADR 0009): only production is public; the rest are
        // the private dev/test zone.
        assert_eq!(EnvClass::Production.node_role(), "prod");
        for c in [EnvClass::Testing, EnvClass::Stable, EnvClass::Ephemeral] {
            assert_eq!(c.node_role(), "private");
            assert!(c.auto_merges(), "{} must auto-merge", c.as_str());
        }
        assert!(!EnvClass::Production.auto_merges());
        assert_eq!(EnvClass::Testing.as_str(), "testing");
        assert_eq!(EnvClass::Testing.env_branch(), "env/testing");
        assert_eq!(EnvClass::ALL.len(), 4);
    }

    #[test]
    fn round_trips_through_serde() {
        for c in EnvClass::ALL {
            let y = serde_yaml::to_string(&c).unwrap();
            let back: EnvClass = serde_yaml::from_str(&y).unwrap();
            assert_eq!(c, back);
            assert_eq!(y.trim(), c.as_str());
        }
    }
}
