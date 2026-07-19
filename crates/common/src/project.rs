//! Project `ops` repo config (`project.yaml`) — §9.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    pub name: String,
    pub members: Vec<Member>,
    pub apps: Vec<AppDecl>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Member {
    /// GitHub username — the identity everywhere (GitHub teams + Tailscale ACLs).
    pub user: String,
    pub role: Role,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Production actions, member management, secrets recipient.
    Admin,
    /// Stable/ephemeral actions only.
    Developer,
}

/// An app declared in `project.yaml`. The bot materializes the repo from the
/// named template if it is missing; removing the entry archives the repo.
///
/// **Monorepo:** by default an app lives in its own repo named after it
/// (`<org>/<name>`). Set `repo` to host several apps in one GitHub repository —
/// apps that share a `repo` value are one monorepo. Such a repo is *not*
/// scaffolded or archived by the platform (bring your own CI); its per-app
/// images are `ghcr.io/<org>/<repo>/<name>`. Ops config stays at `apps/<name>/`
/// regardless — app names are unique within a project.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppDecl {
    pub name: String,
    pub template: String,
    /// GitHub repo hosting this app, if not its own (`<name>`). Shared = monorepo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
}

impl AppDecl {
    /// The GitHub repo hosting this app (its own name unless it's in a monorepo).
    pub fn repo(&self) -> &str {
        self.repo.as_deref().unwrap_or(&self.name)
    }

    /// True if this app shares its repo with others (a monorepo member).
    pub fn is_monorepo(&self) -> bool {
        self.repo.as_deref().is_some_and(|r| r != self.name)
    }

    /// The app's GHCR image base (no tag/digest). A solo app keeps
    /// `ghcr.io/<org>/<name>`. A monorepo member's name carries the repo as a
    /// prefix (`<repo>-<leaf>`) so it stays unique across the project; the GHCR
    /// path already nests under the repo, so the image LEAF is the name with that
    /// prefix stripped — the image stays `ghcr.io/<org>/<repo>/<leaf>` (matching
    /// the repo's build CI) rather than doubling into `.../<repo>/<repo>-<leaf>`.
    pub fn image_base(&self, org: &str) -> String {
        if self.is_monorepo() {
            format!("ghcr.io/{org}/{}/{}", self.repo(), self.image_leaf())
        } else {
            format!("ghcr.io/{org}/{}", self.name)
        }
    }

    /// The image/package leaf for a monorepo member — the app name minus its
    /// `<repo>-` prefix (the CI matrix + GHCR package use this bare leaf). Falls
    /// back to the full name when it isn't prefixed. Meaningless for a solo app.
    pub fn image_leaf(&self) -> &str {
        self.name
            .strip_prefix(&format!("{}-", self.repo()))
            .unwrap_or(&self.name)
    }
}

#[cfg(test)]
mod tests {
    use super::AppDecl;

    fn decl(name: &str, repo: Option<&str>) -> AppDecl {
        AppDecl {
            name: name.into(),
            template: "web-app".into(),
            repo: repo.map(String::from),
        }
    }

    #[test]
    fn solo_app_owns_its_repo_and_flat_image() {
        let a = decl("blog", None);
        assert!(!a.is_monorepo());
        assert_eq!(a.repo(), "blog");
        assert_eq!(a.image_base("acme"), "ghcr.io/acme/blog");
    }

    #[test]
    fn explicit_repo_equal_to_name_is_still_solo() {
        // repo == name is not a monorepo — keeps the flat image.
        let a = decl("blog", Some("blog"));
        assert!(!a.is_monorepo());
        assert_eq!(a.image_base("acme"), "ghcr.io/acme/blog");
    }

    #[test]
    fn monorepo_app_nests_under_repo() {
        // A prefixed name (`<repo>-<leaf>`) nests at the bare leaf — the repo
        // isn't doubled into the path, so the image + CI stay `.../platform/api`.
        let a = decl("platform-api", Some("platform"));
        assert!(a.is_monorepo());
        assert_eq!(a.repo(), "platform");
        assert_eq!(a.image_leaf(), "api");
        assert_eq!(a.image_base("acme"), "ghcr.io/acme/platform/api");
    }

    #[test]
    fn monorepo_leaf_falls_back_when_name_is_unprefixed() {
        // A legacy/bare name (no `<repo>-` prefix) still nests at the full name.
        let a = decl("api", Some("platform"));
        assert_eq!(a.image_leaf(), "api");
        assert_eq!(a.image_base("acme"), "ghcr.io/acme/platform/api");
    }

    #[test]
    fn image_leaf_strips_only_the_repo_prefix() {
        // Repo names with hyphens: only the leading `<repo>-` is stripped.
        let a = decl("my-mono-web", Some("my-mono"));
        assert_eq!(a.image_leaf(), "web");
        assert_eq!(a.image_base("acme"), "ghcr.io/acme/my-mono/web");
    }

    #[test]
    fn repo_field_round_trips_and_defaults() {
        // Omitted `repo` deserializes to None (backward compatible).
        let a: AppDecl = serde_yaml::from_str("name: blog\ntemplate: web-app\n").unwrap();
        assert_eq!(a.repo, None);
        // And a monorepo decl serializes the field back.
        let y = serde_yaml::to_string(&decl("api", Some("platform"))).unwrap();
        assert!(y.contains("repo: platform"), "{y}");
    }
}
