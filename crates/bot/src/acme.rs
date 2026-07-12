//! ACME wildcard certificates for per-project VPN ingress (ADR 0013 phase 2).
//!
//! Each project's private-node Traefik serves `*.{project}.{base_domain}` over
//! the tailnet, so it needs a **browser-trusted** cert. VPN hosts aren't
//! publicly reachable, so issuance is **DNS-01 over Cloudflare** — done here by
//! shelling out to `lego` (native Cloudflare provider; account + renewal state
//! persist under the bot data volume, like the tools in the control-plane
//! image). The bot commits the cert plaintext + the key **age-encrypted** to
//! the platform repo — the ADR 0007 credential bridge; the reconciler decrypts
//! and installs it (phase 3).
//!
//! Credential isolation (§6) holds: the bot touches ACME + Cloudflare + git,
//! and the Cloudflare token reaches only `lego` (via env), never the reconciler.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::AppState;

const LE_STAGING: &str = "https://acme-staging-v02.api.letsencrypt.org/directory";

/// Ensure `*.{project}.{base_domain}` has a fresh cert committed to the platform
/// repo. No-op unless Cloudflare + age recipient + ACME email are all
/// configured. Issues, or renews within 30 days of expiry, via `lego`;
/// age-encrypts the key to the production recipient; commits only when the cert
/// actually changed (so a no-op renew doesn't churn the reconciler). Safe to
/// call every org-sync; non-fatal to the caller.
pub async fn ensure_ingress_cert(state: &AppState, project: &str, base_domain: &str) -> Result<()> {
    let (Some(cf_token), Some(recipient), Some(email)) = (
        state.config.cloudflare_token.clone(),
        state.config.age_production_recipient.clone(),
        state.config.acme_email.clone(),
    ) else {
        tracing::debug!(
            project,
            "Cloudflare/age/ACME-email not all set — skipping ingress cert"
        );
        return Ok(());
    };

    let domain = format!("*.{project}.{base_domain}");
    let lego_dir = state.config.data_dir.join("lego");
    tokio::fs::create_dir_all(&lego_dir).await?;

    let cert = obtain_or_renew(&lego_dir, &cf_token, &email, &domain, state.config.acme_staging)
        .await
        .with_context(|| format!("lego issuance for {domain}"))?;

    // Skip the commit when git already has this exact cert — a renew that found
    // >30 days left changes nothing, and re-committing would nudge the
    // reconciler to re-place an identical cert.
    let org = &state.config.root_org;
    let client = state.github.org_client(org).await?;
    let (crt_path, key_path) = cert_paths(project);
    if let Ok(existing) = crate::platform_api::read_platform_file(&client, org, &crt_path).await {
        if existing.trim() == cert.cert_pem.trim() {
            tracing::debug!(project, "ingress cert unchanged");
            return Ok(());
        }
    }

    let key_enc = crate::cloudflare::age_encrypt(&recipient, &cert.key_pem)
        .await
        .context("age-encrypting ingress key")?;
    // Key first, then cert, so the reconciler (which keys on the .crt) only acts
    // once both files exist (same ordering as ADR 0007 origin certs).
    crate::cloudflare::put_platform_file(
        &client,
        org,
        &key_path,
        &key_enc,
        &format!("ingress: wildcard key for {project}"),
    )
    .await?;
    crate::cloudflare::put_platform_file(
        &client,
        org,
        &crt_path,
        &cert.cert_pem,
        &format!("ingress: wildcard cert for {project}"),
    )
    .await?;
    state.store.log_event("ingress-cert", Some(org), project)?;
    tracing::info!(project, domain, "ingress wildcard certificate committed");
    Ok(())
}

/// Platform-repo paths for a project's ingress wildcard cert + age-encrypted key.
fn cert_paths(project: &str) -> (String, String) {
    (
        format!("platform/ingress-certs/{project}.crt"),
        format!("platform/ingress-certs/{project}.key.age"),
    )
}

struct Certificate {
    cert_pem: String,
    key_pem: String,
}

/// Run `lego run` (first issue) or `lego renew --days 30` (idempotent) and read
/// the resulting PEMs back out of its output directory.
async fn obtain_or_renew(
    dir: &Path,
    cf_token: &str,
    email: &str,
    domain: &str,
    staging: bool,
) -> Result<Certificate> {
    let action = if has_cert(dir, domain).await {
        LegoAction::Renew
    } else {
        LegoAction::Run
    };
    let out = tokio::process::Command::new("lego")
        .args(lego_args(dir, email, domain, staging, action))
        .env("CF_DNS_API_TOKEN", cf_token)
        .output()
        .await
        .context("spawning lego (is it installed?)")?;
    anyhow::ensure!(
        out.status.success(),
        "lego {} failed: {}",
        action.as_str(),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    read_cert(dir, domain).await
}

#[derive(Clone, Copy)]
enum LegoAction {
    Run,
    Renew,
}

impl LegoAction {
    fn as_str(self) -> &'static str {
        match self {
            LegoAction::Run => "run",
            LegoAction::Renew => "renew",
        }
    }
}

/// Build the `lego` argv: global flags, then the subcommand + its flags.
fn lego_args(
    dir: &Path,
    email: &str,
    domain: &str,
    staging: bool,
    action: LegoAction,
) -> Vec<String> {
    let mut args = vec![
        "--accept-tos".into(),
        "--email".into(),
        email.into(),
        "--dns".into(),
        "cloudflare".into(),
        "--domains".into(),
        domain.into(),
        "--path".into(),
        dir.display().to_string(),
    ];
    if staging {
        args.push("--server".into());
        args.push(LE_STAGING.into());
    }
    args.push(action.as_str().into());
    if let LegoAction::Renew = action {
        // Only act inside the renewal window; never block on lego's random sleep.
        args.push("--days".into());
        args.push("30".into());
        args.push("--no-random-sleep".into());
    }
    args
}

/// lego writes `certificates/<sanitized>.{crt,key}`, with the wildcard `*`
/// rewritten to `_` (e.g. `*.p.majksa.net` → `_.p.majksa.net`).
fn cert_basename(domain: &str) -> String {
    domain.replace('*', "_")
}

async fn has_cert(dir: &Path, domain: &str) -> bool {
    tokio::fs::metadata(crt_file(dir, domain)).await.is_ok()
}

fn crt_file(dir: &Path, domain: &str) -> PathBuf {
    dir.join("certificates")
        .join(format!("{}.crt", cert_basename(domain)))
}

fn key_file(dir: &Path, domain: &str) -> PathBuf {
    dir.join("certificates")
        .join(format!("{}.key", cert_basename(domain)))
}

async fn read_cert(dir: &Path, domain: &str) -> Result<Certificate> {
    Ok(Certificate {
        cert_pem: tokio::fs::read_to_string(crt_file(dir, domain))
            .await
            .context("reading lego cert")?,
        key_pem: tokio::fs::read_to_string(key_file(dir, domain))
            .await
            .context("reading lego key")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cert_paths_are_per_project() {
        let (crt, key) = cert_paths("zpevnik");
        assert_eq!(crt, "platform/ingress-certs/zpevnik.crt");
        assert_eq!(key, "platform/ingress-certs/zpevnik.key.age");
    }

    #[test]
    fn wildcard_maps_to_legos_underscore_filename() {
        assert_eq!(cert_basename("*.p.majksa.net"), "_.p.majksa.net");
        let f = crt_file(Path::new("/data/lego"), "*.p.majksa.net");
        assert_eq!(
            f,
            Path::new("/data/lego/certificates/_.p.majksa.net.crt")
        );
    }

    #[test]
    fn run_args_have_no_renew_flags() {
        let args = lego_args(
            Path::new("/d"),
            "a@b.c",
            "*.p.majksa.net",
            false,
            LegoAction::Run,
        );
        assert!(args.contains(&"run".to_string()));
        assert!(!args.iter().any(|a| a == "--days"));
        assert!(!args.iter().any(|a| a == "--server")); // production by default
        // globals precede the subcommand
        let run = args.iter().position(|a| a == "run").unwrap();
        let dns = args.iter().position(|a| a == "--dns").unwrap();
        assert!(dns < run);
    }

    #[test]
    fn renew_args_are_windowed_and_staging_switches_server() {
        let args = lego_args(
            Path::new("/d"),
            "a@b.c",
            "*.p.majksa.net",
            true,
            LegoAction::Renew,
        );
        assert!(args.contains(&"renew".to_string()));
        assert!(args.contains(&"--days".to_string()));
        assert!(args.contains(&"30".to_string()));
        assert!(args.contains(&"--no-random-sleep".to_string()));
        assert!(args.contains(&LE_STAGING.to_string()));
        // --days is a renew subcommand flag: it must come after `renew`
        let renew = args.iter().position(|a| a == "renew").unwrap();
        let days = args.iter().position(|a| a == "--days").unwrap();
        assert!(renew < days);
    }
}
