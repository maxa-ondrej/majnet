//! Setup service configuration — twelve-factor, from environment variables.

use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    /// Public wizard listener — first run only, closed by /finish.
    pub listen_public: String,
    /// WG-internal listener (enrollment API stays available after setup).
    /// Bind to the main node's WG IP in production.
    pub listen_internal: String,
    /// Config root: token, state, bot.env, PEM, PKI CA, done marker.
    pub etc_dir: PathBuf,
    /// The majnet checkout (installer clones it): `bootstrap/` payload +
    /// `platform-seed/` live here.
    pub repo_dir: PathBuf,
    /// Bot WG-internal API (platform seed + node upsert, ADR 0004).
    pub bot_url: String,
}

impl Config {
    pub fn from_env() -> Self {
        let var =
            |name: &str, default: &str| std::env::var(name).unwrap_or_else(|_| default.into());
        Self {
            listen_public: var("MAJNET_SETUP_LISTEN_PUBLIC", "0.0.0.0:7600"),
            listen_internal: var("MAJNET_SETUP_LISTEN_INTERNAL", "127.0.0.1:7601"),
            etc_dir: var("MAJNET_ETC_DIR", "/etc/majnet").into(),
            repo_dir: var("MAJNET_REPO_DIR", "/opt/majnet").into(),
            bot_url: var("MAJNET_BOT_INTERNAL_URL", "http://10.88.0.1:8081")
                .trim_end_matches('/')
                .to_string(),
        }
    }

    pub fn token_path(&self) -> PathBuf {
        self.etc_dir.join("setup-token")
    }
    pub fn state_path(&self) -> PathBuf {
        self.etc_dir.join("setup-state.json")
    }
    pub fn done_path(&self) -> PathBuf {
        self.etc_dir.join("setup-done")
    }
    /// Enrollment SSH keypair (installer generates; pubkey shown in the UI).
    pub fn ssh_key_path(&self) -> PathBuf {
        self.etc_dir.join("enroll_ed25519")
    }
    /// CA + issued certs (gen-certs.sh output). The CA key lives here — the
    /// provisioner credential class (ADR 0004).
    pub fn pki_dir(&self) -> PathBuf {
        self.etc_dir.join("pki-ca")
    }
    pub fn bot_env_path(&self) -> PathBuf {
        self.etc_dir.join("bot.env")
    }
    pub fn app_pem_path(&self) -> PathBuf {
        self.etc_dir.join("github-app.pem")
    }
}
