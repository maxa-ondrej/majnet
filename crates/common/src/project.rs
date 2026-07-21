//! Project `ops` repo config (`project.yaml`) — §9.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    pub name: String,
    pub members: Vec<Member>,
    pub apps: Vec<AppDecl>,
    /// Project-owned "service" apps (ADR 0021): an external image + config with
    /// no source repo, no CI, and one environment (chosen by `exposure`). Absent
    /// = none. Their manifest lives at `apps/<name>/` like any app; this list
    /// just tracks them (so the dashboard lists them + org-sync leaves them
    /// alone — they're not `apps`, so no repo is created/archived for them).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<ServiceDecl>,
}

/// A project-owned service (ADR 0021). The full manifest (image, env, ingress,
/// secrets, volumes, resources, database) is at `apps/<name>/base.yaml` + the
/// single overlay for `exposure.class()`; this entry records that it's a service
/// and where it runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceDecl {
    pub name: String,
    pub exposure: Exposure,
    /// Build repo hosting this service's image(s), if one lives in the managed
    /// org (e.g. `observability` for config-baked backend images). A service has
    /// no scaffolded repo of its own, but org-sync would otherwise archive any
    /// unreferenced repo in the org — naming it here keeps it active (like a
    /// monorepo `repo:`). Absent ⇒ the image is hosted elsewhere (nothing to
    /// keep active). Several services may share one build repo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
}

/// Where a service runs + how it's reached (ADR 0021). Maps to an `EnvClass` so
/// a service reuses the class's static placement + ingress behavior:
/// - `public` → `production`: prod node, Cloudflare edge, a custom domain.
/// - `internal` → `stable`: private node, tailnet auto-host, no public exposure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Exposure {
    Public,
    Internal,
}

impl Exposure {
    /// The env class this exposure renders/converges as.
    pub fn class(&self) -> crate::EnvClass {
        match self {
            Exposure::Public => crate::EnvClass::Production,
            Exposure::Internal => crate::EnvClass::Stable,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Exposure::Public => "public",
            Exposure::Internal => "internal",
        }
    }
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
    /// Per-app release policy (ADR 0020). Absent ⇒ today's repo-wide `vX.Y.Z`
    /// releasing; present with a `scope` ⇒ per-app tags `@<scope>/<leaf>@<ver>`
    /// and (optionally) autorelease on merge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<ReleaseConfig>,
}

/// Per-app release policy (ADR 0020) — GitOps config, written by the dashboard.
///
/// A **scope** opts the app into per-app release tags (`@<scope>/<leaf>@<ver>`,
/// Changesets-style) instead of the repo-wide `vX.Y.Z` line; the leaf is the
/// app's image leaf (`AppDecl::image_leaf`). Without a scope the app releases
/// repo-wide, exactly as before. **Autorelease** cuts a release automatically on
/// a push to `main` that touches one of `paths`; the bump is `patch` (always) or
/// `auto` (conventional commits).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseConfig {
    /// Tag scope for per-app releases (e.g. `sideline` → `@sideline/<leaf>@<ver>`).
    /// `Some` ⇒ per-app mode; `None` ⇒ repo-wide `vX.Y.Z` (default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Automatic releasing on merge to `main`. `off` (default) never auto-cuts.
    #[serde(default)]
    pub autorelease: Autorelease,
    /// Path globs; a push touching any of these autoreleases this app (only
    /// consulted when `autorelease` is `patch`/`auto`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    /// Override the conventional-commit → semver-bump mapping for `auto` bumps +
    /// the changelog (ADR 0020): commit `type` → the bump it triggers. A breaking
    /// change (`type!` / `BREAKING CHANGE`) is always major regardless; unlisted
    /// types are ignored. Absent ⇒ the default (`feat: minor`, `fix: patch`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bumps: Option<BTreeMap<String, Bump>>,
}

/// A semver bump level (ADR 0020) — the value in a `release.bumps` mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Bump {
    Major,
    Minor,
    Patch,
}

impl Bump {
    pub fn as_str(&self) -> &'static str {
        match self {
            Bump::Major => "major",
            Bump::Minor => "minor",
            Bump::Patch => "patch",
        }
    }
    /// Precedence for picking the strongest bump across commits (major wins).
    pub fn rank(&self) -> u8 {
        match self {
            Bump::Major => 3,
            Bump::Minor => 2,
            Bump::Patch => 1,
        }
    }
}

/// The default conventional-commit → bump mapping when an app configures none:
/// `feat` → minor, `fix` → patch (breaking is always major; other types ignored).
pub fn default_bump_rules() -> BTreeMap<String, Bump> {
    BTreeMap::from([
        ("feat".to_string(), Bump::Minor),
        ("fix".to_string(), Bump::Patch),
    ])
}

/// Autorelease bump strategy for an app (ADR 0020).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Autorelease {
    /// No automatic releasing — cuts are manual (dashboard) only.
    #[default]
    Off,
    /// Every autorelease bumps the patch component.
    Patch,
    /// Derive the bump from conventional-commit messages since the last release.
    Auto,
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

    /// The configured per-app release scope, if any (ADR 0020).
    pub fn release_scope(&self) -> Option<&str> {
        self.release.as_ref().and_then(|r| r.scope.as_deref())
    }

    /// True when this app releases with per-app scoped tags (a `scope` is set)
    /// rather than the repo-wide `vX.Y.Z` line.
    pub fn is_per_app_release(&self) -> bool {
        self.release_scope().is_some()
    }

    /// The git tag for releasing this app at `version` (which already carries any
    /// `v`/bare prefix). Per-app: `@<scope>/<leaf>@<version>`; repo-wide: just
    /// `<version>`. The leaf is `image_leaf()`, matching the nested image + CI.
    pub fn release_tag(&self, version: &str) -> String {
        match self.release_scope() {
            Some(scope) => format!("@{scope}/{}@{version}", self.image_leaf()),
            None => version.to_string(),
        }
    }

    /// This app's autorelease strategy (ADR 0020); `Off` when unconfigured.
    pub fn autorelease_mode(&self) -> Autorelease {
        self.release
            .as_ref()
            .map(|r| r.autorelease)
            .unwrap_or_default()
    }

    /// The effective conventional-commit → bump mapping for this app: its
    /// configured `release.bumps`, else the default (`feat: minor`, `fix: patch`).
    pub fn bump_rules(&self) -> BTreeMap<String, Bump> {
        self.release
            .as_ref()
            .and_then(|r| r.bumps.clone())
            .unwrap_or_else(default_bump_rules)
    }

    /// Path globs that trigger an autorelease for this app.
    pub fn release_paths(&self) -> &[String] {
        self.release
            .as_ref()
            .map(|r| r.paths.as_slice())
            .unwrap_or(&[])
    }

    /// The key a release/draft is tracked under: the app itself in per-app mode
    /// (each app releases independently), else the shared repo (one repo-wide
    /// version line). Used to key drafts and compute the "last version".
    pub fn release_unit(&self) -> &str {
        if self.is_per_app_release() {
            &self.name
        } else {
            self.repo()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AppDecl, Autorelease, ReleaseConfig};

    fn decl(name: &str, repo: Option<&str>) -> AppDecl {
        AppDecl {
            name: name.into(),
            template: "web-app".into(),
            repo: repo.map(String::from),
            release: None,
        }
    }

    fn with_release(mut a: AppDecl, r: ReleaseConfig) -> AppDecl {
        a.release = Some(r);
        a
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
        assert_eq!(a.release, None);
        // And a monorepo decl serializes the field back.
        let y = serde_yaml::to_string(&decl("api", Some("platform"))).unwrap();
        assert!(y.contains("repo: platform"), "{y}");
        // A decl with no release block serializes without the key (clean diffs).
        let y = serde_yaml::to_string(&decl("blog", None)).unwrap();
        assert!(!y.contains("release"), "{y}");
    }

    #[test]
    fn no_release_block_is_repo_wide_and_off() {
        let a = decl("api", Some("platform"));
        assert!(!a.is_per_app_release());
        assert_eq!(a.release_scope(), None);
        assert_eq!(a.autorelease_mode(), Autorelease::Off);
        assert!(a.release_paths().is_empty());
        assert_eq!(a.release_unit(), "platform"); // the repo — one shared line
                                                  // The release tag is just the (already-prefixed) version, repo-wide.
        assert_eq!(a.release_tag("v1.2.3"), "v1.2.3");
        assert_eq!(a.release_tag("0.30.6"), "0.30.6");
    }

    #[test]
    fn scope_enables_per_app_scoped_tags() {
        // sideline-server in repo `sideline`, scope `sideline` → leaf `server`.
        let a = with_release(
            decl("sideline-server", Some("sideline")),
            ReleaseConfig {
                scope: Some("sideline".into()),
                autorelease: Autorelease::Auto,
                paths: vec!["applications/server/**".into()],
                bumps: None,
            },
        );
        assert!(a.is_per_app_release());
        assert_eq!(a.release_scope(), Some("sideline"));
        assert_eq!(a.image_leaf(), "server");
        assert_eq!(a.release_tag("v0.39.1"), "@sideline/server@v0.39.1");
        // Prefix is preserved by the caller (bare version → bare tag).
        assert_eq!(a.release_tag("0.39.1"), "@sideline/server@0.39.1");
        assert_eq!(a.autorelease_mode(), Autorelease::Auto);
        assert_eq!(a.release_paths(), ["applications/server/**"]);
        // Per-app ⇒ the release unit is the app itself, not the repo.
        assert_eq!(a.release_unit(), "sideline-server");
    }

    #[test]
    fn scope_may_differ_from_repo_name() {
        // A scope unrelated to the repo name still forms the tag from the leaf.
        let a = with_release(
            decl("mono-web", Some("mono")),
            ReleaseConfig {
                scope: Some("@acme".into()),
                ..Default::default()
            },
        );
        assert_eq!(a.release_tag("v2.0.0"), "@@acme/web@v2.0.0");
    }

    #[test]
    fn release_block_round_trips() {
        let y = "name: sideline-bot\ntemplate: byo\nrepo: sideline\n\
                 release:\n  scope: sideline\n  autorelease: patch\n  \
                 paths:\n    - applications/bot/**\n";
        let a: AppDecl = serde_yaml::from_str(y).unwrap();
        assert_eq!(a.release_scope(), Some("sideline"));
        assert_eq!(a.autorelease_mode(), Autorelease::Patch);
        assert_eq!(a.release_paths(), ["applications/bot/**"]);
        // Round-trips back with the release block present.
        let out = serde_yaml::to_string(&a).unwrap();
        assert!(out.contains("autorelease: patch"), "{out}");
    }

    #[test]
    fn services_default_empty_and_map_exposure_to_class() {
        use super::{Exposure, ProjectConfig};
        // Omitted `services:` → empty (backward compatible).
        let p: ProjectConfig = serde_yaml::from_str("name: proj\nmembers: []\napps: []\n").unwrap();
        assert!(p.services.is_empty());
        // Exposure → class mapping.
        assert_eq!(Exposure::Public.class(), crate::EnvClass::Production);
        assert_eq!(Exposure::Internal.class(), crate::EnvClass::Stable);
        // A services block round-trips.
        let y = "name: proj\nmembers: []\napps: []\n\
                 services:\n  - name: signoz\n    exposure: internal\n";
        let p: ProjectConfig = serde_yaml::from_str(y).unwrap();
        assert_eq!(p.services.len(), 1);
        assert_eq!(p.services[0].name, "signoz");
        assert_eq!(p.services[0].exposure, Exposure::Internal);
        let out = serde_yaml::to_string(&p).unwrap();
        assert!(out.contains("exposure: internal"), "{out}");
        // No `repo:` by default (backward compatible + not serialized when None).
        assert_eq!(p.services[0].repo, None);
        assert!(!out.contains("repo:"), "{out}");
    }

    #[test]
    fn service_build_repo_round_trips() {
        use super::ProjectConfig;
        // A service naming its image build repo (kept active by org-sync).
        let y = "name: majnet\nmembers: []\napps: []\n\
                 services:\n  - name: otel-collector\n    exposure: internal\n    repo: observability\n";
        let p: ProjectConfig = serde_yaml::from_str(y).unwrap();
        assert_eq!(p.services[0].repo.as_deref(), Some("observability"));
        let out = serde_yaml::to_string(&p).unwrap();
        assert!(out.contains("repo: observability"), "{out}");
    }

    #[test]
    fn bump_rules_default_and_override() {
        use super::Bump;
        // No config → the default mapping.
        let d = decl("api", Some("mono"));
        let r = d.bump_rules();
        assert_eq!(r.get("feat"), Some(&Bump::Minor));
        assert_eq!(r.get("fix"), Some(&Bump::Patch));
        assert_eq!(r.get("chore"), None);
        // Override round-trips + replaces the default entirely.
        let y = "name: api\ntemplate: byo\nrepo: mono\n\
                 release:\n  scope: mono\n  bumps:\n    feat: minor\n    fix: patch\n    perf: patch\n";
        let a: AppDecl = serde_yaml::from_str(y).unwrap();
        let r = a.bump_rules();
        assert_eq!(r.get("perf"), Some(&Bump::Patch));
        assert_eq!(r.len(), 3);
        let out = serde_yaml::to_string(&a).unwrap();
        assert!(out.contains("perf: patch"), "{out}");
    }

    #[test]
    fn autorelease_defaults_to_off_when_omitted() {
        // A release block with only a scope leaves autorelease off + paths empty.
        let a: AppDecl =
            serde_yaml::from_str("name: web\ntemplate: byo\nrepo: mono\nrelease:\n  scope: mono\n")
                .unwrap();
        assert!(a.is_per_app_release());
        assert_eq!(a.autorelease_mode(), Autorelease::Off);
    }
}
