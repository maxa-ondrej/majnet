//! Role resolution for the human-facing write APIs (§16, phase 5).
//!
//! Identity arrives as the `Tailscale-User-Login` header. The trust anchor
//! is `tailscale serve` in front of the dashboard: it sets the header itself
//! (client-supplied values never pass through), and the WG-internal
//! listeners are unreachable for humans except via that path. A request
//! **without** the header is a platform component on the WG mesh (or an
//! operator's break-glass curl from a node) — trusted by the network
//! boundary, audited as `infra`. A request **with** the header is a human:
//! `people.yaml` maps the Tailscale login to a GitHub user (+ the
//! platform-admin flag), `project.yaml` supplies the per-project role.

use crate::platform::PeopleFile;
use crate::project::{ProjectConfig, Role};
use anyhow::{bail, Result};

/// Who is acting, resolved from the header + platform config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Actor {
    /// No identity header: a platform component or node-local break-glass.
    Infra,
    /// A human, identified via people.yaml.
    Human {
        github: String,
        platform_admin: bool,
    },
}

impl Actor {
    /// Audit-log label.
    pub fn label(&self) -> &str {
        match self {
            Actor::Infra => "infra",
            Actor::Human { github, .. } => github,
        }
    }
}

/// Map the (optional) Tailscale login to an actor.
pub fn identify(ts_login: Option<&str>, people: &PeopleFile) -> Result<Actor> {
    let Some(login) = ts_login else {
        return Ok(Actor::Infra);
    };
    match people.people.iter().find(|p| p.tailscale == login) {
        Some(p) => Ok(Actor::Human {
            github: p.github.clone(),
            platform_admin: p.admin,
        }),
        None => bail!("{login} is not in people.yaml"),
    }
}

/// Enforce `min_role` on a project. Infra and platform admins always pass;
/// humans need a project.yaml membership at or above the required role
/// (admin ≥ developer, §9).
pub fn require_role(actor: &Actor, project: Option<&ProjectConfig>, min_role: Role) -> Result<()> {
    let github = match actor {
        Actor::Infra => return Ok(()),
        Actor::Human {
            platform_admin: true,
            ..
        } => return Ok(()),
        Actor::Human { github, .. } => github,
    };
    let Some(project) = project else {
        bail!("{github} is not a platform admin and the project config is unavailable");
    };
    let Some(member) = project.members.iter().find(|m| &m.user == github) else {
        bail!("{github} is not a member of {}", project.name);
    };
    if min_role == Role::Admin && member.role != Role::Admin {
        bail!(
            "{github} is a developer on {} — admin required",
            project.name
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::{AppDecl, Member};

    fn people() -> PeopleFile {
        serde_yaml::from_str(
            "people:\n\
             - {github: alice, tailscale: alice@example.com, admin: true}\n\
             - {github: bob, tailscale: bob@example.com}\n\
             - {github: carol, tailscale: carol@example.com}\n",
        )
        .unwrap()
    }

    fn project() -> ProjectConfig {
        ProjectConfig {
            name: "zpevnik".into(),
            members: vec![
                Member {
                    user: "bob".into(),
                    role: Role::Admin,
                },
                Member {
                    user: "carol".into(),
                    role: Role::Developer,
                },
            ],
            apps: Vec::<AppDecl>::new(),
            services: vec![],
        }
    }

    #[test]
    fn no_header_is_infra_and_passes_everything() {
        let actor = identify(None, &people()).unwrap();
        assert_eq!(actor, Actor::Infra);
        assert!(require_role(&actor, None, Role::Admin).is_ok());
    }

    #[test]
    fn unknown_login_is_rejected() {
        assert!(identify(Some("mallory@example.com"), &people()).is_err());
    }

    #[test]
    fn platform_admin_passes_without_membership() {
        let actor = identify(Some("alice@example.com"), &people()).unwrap();
        assert!(require_role(&actor, Some(&project()), Role::Admin).is_ok());
        assert!(require_role(&actor, None, Role::Admin).is_ok());
    }

    #[test]
    fn project_roles_are_enforced() {
        let bob = identify(Some("bob@example.com"), &people()).unwrap();
        let carol = identify(Some("carol@example.com"), &people()).unwrap();
        // Project admin: everything on their project.
        assert!(require_role(&bob, Some(&project()), Role::Admin).is_ok());
        // Developer: developer actions yes, admin actions no.
        assert!(require_role(&carol, Some(&project()), Role::Developer).is_ok());
        assert!(require_role(&carol, Some(&project()), Role::Admin).is_err());
        // Non-member of this project.
        let other = ProjectConfig {
            name: "other".into(),
            members: vec![],
            apps: vec![],
            services: vec![],
        };
        assert!(require_role(&bob, Some(&other), Role::Developer).is_err());
    }
}
