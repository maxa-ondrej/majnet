//! GitHub App auth (§11.1): app-level JWT client plus a per-org cache of
//! installation clients + tokens. Installation tokens live 60 min; we refresh
//! after 50.

use anyhow::{Context, Result};
use octocrab::models::InstallationId;
use octocrab::Octocrab;
use secrecy::SecretString;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const TOKEN_TTL: Duration = Duration::from_secs(50 * 60);

struct CachedInstallation {
    client: Octocrab,
    token: SecretString,
    fetched_at: Instant,
}

pub struct GitHub {
    app: Octocrab,
    cache: Mutex<HashMap<String, CachedInstallation>>,
}

impl GitHub {
    pub fn new(app_id: u64, private_key_pem: &[u8]) -> Result<Self> {
        let key = jsonwebtoken::EncodingKey::from_rsa_pem(private_key_pem)
            .context("GitHub App private key is not a valid RSA PEM")?;
        let app = Octocrab::builder().app(app_id.into(), key).build()?;
        Ok(Self { app, cache: Mutex::new(HashMap::new()) })
    }

    /// Installation-scoped client for an org (cached).
    pub async fn org_client(&self, org: &str) -> Result<Octocrab> {
        Ok(self.org_client_and_token(org).await?.0)
    }

    /// Installation client + raw token (the token authenticates tarball
    /// downloads, which go through reqwest rather than octocrab).
    pub async fn org_client_and_token(&self, org: &str) -> Result<(Octocrab, SecretString)> {
        let mut cache = self.cache.lock().await;
        if let Some(hit) = cache.get(org) {
            if hit.fetched_at.elapsed() < TOKEN_TTL {
                return Ok((hit.client.clone(), hit.token.clone()));
            }
        }

        let installation = self
            .app
            .apps()
            .get_org_installation(org)
            .await
            .with_context(|| format!("GitHub App is not installed on org '{org}'"))?;
        let (client, token) = self.installation_client(installation.id).await?;
        cache.insert(
            org.to_string(),
            CachedInstallation { client: client.clone(), token: token.clone(), fetched_at: Instant::now() },
        );
        Ok((client, token))
    }

    async fn installation_client(&self, id: InstallationId) -> Result<(Octocrab, SecretString)> {
        let (client, token) = self.app.installation_and_token(id).await?;
        Ok((client, token))
    }
}
