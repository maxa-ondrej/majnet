//! Managed databases (§15): per-project/app logical DBs + users on the
//! engine instance of the app's trust zone (prod DBs on the prod node, dev
//! DBs on the private node) — engines listen on Docker networks only, never
//! any VPN.
//!
//! **Stateless credentials:** passwords are derived as
//! `HMAC-SHA256(master_key, "engine:project:app:class")` — the reconciler
//! stores nothing (it carries no state git doesn't), and node recovery
//! reproduces the same credentials. Master key: `db-master.key` next to the
//! age keys, generated once at install.
//!
//! Engine containers (`majnet-postgres`, `majnet-mariadb`, `majnet-valkey`,
//! `majnet-mongodb`) are platform services (§10 `platform/` manifests) —
//! provisioning execs into them and attaches them to the project's network
//! so the app can resolve them by container name.
//!
//! Valkey has no per-user keyspace primitive, so its ACL users share one
//! keyspace: the credential is authentication, not isolation (zone trust
//! still applies, same as the design assumes for engines generally).

use anyhow::{Context, Result};
use bollard::query_parameters as qp;
use bollard::Docker;
use hmac::{Hmac, Mac};
use majnet_common::manifest::DbEngine;
use majnet_common::EnvClass;
use sha2::Sha256;

use crate::config::Config;
use crate::deploy::network_name;

/// Ensure DB + user exist; returns env vars to inject into the app.
pub async fn ensure(
    config: &Config,
    docker: &Docker,
    project: &str,
    app: &str,
    class: EnvClass,
    engine: DbEngine,
    dry_run: bool,
) -> Result<Vec<(String, String)>> {
    let name = db_name(project, app, class);
    let password = derive_password(config, engine, project, app, class)?;
    let container = engine_container(engine);

    let env = match engine {
        DbEngine::Postgres => vec![
            (
                "DATABASE_URL".into(),
                format!("postgres://{name}:{password}@{container}:5432/{name}"),
            ),
            ("PGHOST".into(), container.to_string()),
            ("PGDATABASE".into(), name.clone()),
            ("PGUSER".into(), name.clone()),
            ("PGPASSWORD".into(), password.clone()),
        ],
        DbEngine::Mariadb => vec![(
            "DATABASE_URL".into(),
            format!("mysql://{name}:{password}@{container}:3306/{name}"),
        )],
        DbEngine::Valkey => vec![
            (
                "DATABASE_URL".into(),
                format!("redis://{name}:{password}@{container}:6379/0"),
            ),
            (
                "REDIS_URL".into(),
                format!("redis://{name}:{password}@{container}:6379/0"),
            ),
        ],
        DbEngine::Mongodb => vec![(
            "DATABASE_URL".into(),
            format!("mongodb://{name}:{password}@{container}:27017/{name}?authSource={name}"),
        )],
    };

    if dry_run {
        tracing::info!(
            project,
            app,
            ?engine,
            db = name,
            "DRY RUN: would provision database"
        );
        return Ok(env);
    }

    // The engine must be reachable from the app's project network.
    connect_engine_to_network(docker, container, project).await?;

    let script = match engine {
        DbEngine::Postgres => format!(
            r#"psql -U postgres -tc "SELECT 1 FROM pg_roles WHERE rolname='{name}'" | grep -q 1 || psql -U postgres -c "CREATE ROLE \"{name}\" LOGIN PASSWORD '{password}'"
psql -U postgres -c "ALTER ROLE \"{name}\" WITH PASSWORD '{password}'"
psql -U postgres -tc "SELECT 1 FROM pg_database WHERE datname='{name}'" | grep -q 1 || psql -U postgres -c "CREATE DATABASE \"{name}\" OWNER \"{name}\"""#
        ),
        DbEngine::Mariadb => format!(
            r#"mariadb -uroot -p"$(cat /run/secrets/mariadb-root)" -e "CREATE DATABASE IF NOT EXISTS \`{name}\`; CREATE USER IF NOT EXISTS '{name}'@'%' IDENTIFIED BY '{password}'; ALTER USER '{name}'@'%' IDENTIFIED BY '{password}'; GRANT ALL ON \`{name}\`.* TO '{name}'@'%';""#
        ),
        DbEngine::Valkey => format!(
            r#"AUTH="$(cat /run/secrets/valkey-root)"
valkey-cli -a "$AUTH" --no-auth-warning ACL SETUSER {name} on '>{password}' '~*' '&*' '+@all' '-@admin'
valkey-cli -a "$AUTH" --no-auth-warning ACL SAVE"#
        ),
        DbEngine::Mongodb => format!(
            r#"mongosh --quiet -u root -p "$(cat /run/secrets/mongodb-root)" --authenticationDatabase admin --eval 'const d = db.getSiblingDB("{name}"); if (d.getUser("{name}")) {{ d.updateUser("{name}", {{ pwd: "{password}" }}); }} else {{ d.createUser({{ user: "{name}", pwd: "{password}", roles: [{{ role: "dbOwner", db: "{name}" }}] }}); }}'"#
        ),
    };
    exec(docker, container, &script)
        .await
        .with_context(|| format!("provisioning {name} on {container}"))?;
    tracing::debug!(project, app, db = name, "database ensured");
    Ok(env)
}

/// Restore a dump into the app's managed database (ADR 0010 phase 3). The DB +
/// user must already exist (`ensure` first). Ships the dump into the engine
/// container and runs the native client as the superuser, targeting the app's
/// logical DB. Forward-only: a partial restore left behind is the operator's to
/// reset (drop/recreate) before retrying.
pub async fn restore(
    docker: &Docker,
    project: &str,
    app: &str,
    class: EnvClass,
    engine: DbEngine,
    dump: &[u8],
) -> Result<()> {
    let db = db_name(project, app, class);
    let container = engine_container(engine);
    let script = restore_script(engine, &db)?;

    upload_dump(docker, container, dump)
        .await
        .with_context(|| format!("uploading dump to {container}"))?;
    let result = exec(docker, container, &script)
        .await
        .with_context(|| format!("restoring dump into {db}"));
    // The dump carries data — always remove it, success or not.
    let _ = exec(docker, container, &format!("rm -f {RESTORE_PATH}")).await;
    result
}

const RESTORE_PATH: &str = "/tmp/majnet-restore.dump";

/// The superuser restore command per engine (SQL text dumps). Postgres and
/// MariaDB only for v1; Mongo/Valkey return a clear error.
fn restore_script(engine: DbEngine, db: &str) -> Result<String> {
    Ok(match engine {
        DbEngine::Postgres => {
            format!(r#"psql -U postgres -v ON_ERROR_STOP=1 -d "{db}" -f {RESTORE_PATH}"#)
        }
        DbEngine::Mariadb => format!(
            r#"mariadb -uroot -p"$(cat /run/secrets/mariadb-root)" "{db}" < {RESTORE_PATH}"#
        ),
        DbEngine::Mongodb | DbEngine::Valkey => anyhow::bail!(
            "data restore for {engine:?} is not supported yet (v1: postgres + mariadb SQL dumps)"
        ),
    })
}

/// Ship `dump` into `container` at `RESTORE_PATH` via `put_archive`.
async fn upload_dump(docker: &Docker, container: &str, dump: &[u8]) -> Result<()> {
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(dump.len() as u64);
    header.set_mode(0o600);
    header.set_cksum();
    let name = RESTORE_PATH.trim_start_matches('/'); // tar paths are relative to /
    builder.append_data(&mut header, name, dump)?;
    let tarball = builder.into_inner()?;
    docker
        .upload_to_container(
            container,
            Some(qp::UploadToContainerOptions {
                path: "/".into(),
                ..Default::default()
            }),
            bollard::body_full(tarball.into()),
        )
        .await
        .context("uploading dump archive")?;
    Ok(())
}

/// `<project>_<app>_<class>` with `-` → `_` (identifier-safe, ≤63 chars for pg).
pub fn db_name(project: &str, app: &str, class: EnvClass) -> String {
    let mut name = format!("{project}_{app}_{}", class.as_str()).replace('-', "_");
    name.truncate(63);
    name
}

fn derive_password(
    config: &Config,
    engine: DbEngine,
    project: &str,
    app: &str,
    class: EnvClass,
) -> Result<String> {
    hmac16(
        config,
        &format!("{engine:?}:{project}:{app}:{}", class.as_str()),
    )
}

/// The engine's superuser password — the same stateless HMAC derivation as
/// per-app users, domain-separated by the `root:` prefix. `platform::ensure_engine`
/// seeds the engine's root-secret file with this so a rebuilt node (fresh
/// volume) reproduces it; the provisioning scripts read it from that file.
pub fn root_password(config: &Config, engine: DbEngine) -> Result<String> {
    hmac16(config, &format!("root:{engine:?}"))
}

/// `HMAC-SHA256(db-master.key, msg)`, first 16 bytes hex — the stateless
/// credential primitive (§15). The master key lives next to the age keys.
fn hmac16(config: &Config, msg: &str) -> Result<String> {
    let key_path = config.age_key_dir.join("db-master.key");
    let master = std::fs::read(&key_path).with_context(|| {
        format!(
            "missing DB master key {} (generate: openssl rand -hex 32 > …)",
            key_path.display()
        )
    })?;
    Ok(hmac16_with(&master, msg))
}

/// The pure derivation: `HMAC-SHA256(master, msg)`, first 16 bytes hex.
fn hmac16_with(master: &[u8], msg: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(master).expect("any key length");
    mac.update(msg.as_bytes());
    hex::encode(&mac.finalize().into_bytes()[..16])
}

/// The engine's container name — the contract shared with `platform::ensure_engine`
/// (which creates it) and `db::ensure` (which execs into it).
pub fn engine_container(engine: DbEngine) -> &'static str {
    match engine {
        DbEngine::Postgres => "majnet-postgres",
        DbEngine::Mariadb => "majnet-mariadb",
        DbEngine::Valkey => "majnet-valkey",
        DbEngine::Mongodb => "majnet-mongodb",
    }
}

async fn connect_engine_to_network(docker: &Docker, container: &str, project: &str) -> Result<()> {
    let inspect = docker
        .inspect_container(container, None::<qp::InspectContainerOptions>)
        .await
        .with_context(|| {
            format!(
                "engine container '{container}' not found — deploy it via platform manifests first"
            )
        })?;
    let network = network_name(project);
    let attached = inspect
        .network_settings
        .and_then(|s| s.networks)
        .is_some_and(|n| n.contains_key(&network));
    if attached {
        return Ok(());
    }
    docker
        .connect_network(
            &network,
            bollard::models::NetworkConnectRequest {
                container: container.into(),
                ..Default::default()
            },
        )
        .await
        .with_context(|| format!("attaching {container} to {network}"))?;
    Ok(())
}

async fn exec(docker: &Docker, container: &str, script: &str) -> Result<()> {
    use futures_util::StreamExt;
    let exec = docker
        .create_exec(
            container,
            bollard::models::ExecConfig {
                cmd: Some(vec!["sh".into(), "-ec".into(), script.into()]),
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                ..Default::default()
            },
        )
        .await?;
    let mut collected = String::new();
    if let bollard::exec::StartExecResults::Attached {
        output: mut stream, ..
    } = docker
        .start_exec(&exec.id, None::<bollard::exec::StartExecOptions>)
        .await?
    {
        while let Some(chunk) = stream.next().await {
            collected.push_str(&chunk?.to_string());
        }
    }
    let inspect = docker.inspect_exec(&exec.id).await?;
    let code = inspect.exit_code.unwrap_or(-1);
    anyhow::ensure!(
        code == 0,
        "provisioning script exited {code}: {}",
        collected.trim()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{db_name, hmac16_with};
    use majnet_common::EnvClass;

    #[test]
    fn derived_credentials_are_deterministic_and_domain_separated() {
        let master = b"0123456789abcdef0123456789abcdef";
        // Deterministic: same inputs → same secret (node recovery reproduces it).
        assert_eq!(
            hmac16_with(master, "root:Postgres"),
            hmac16_with(master, "root:Postgres")
        );
        // A root secret never collides with an app user, nor across engines,
        // nor across a different master key.
        let root_pg = hmac16_with(master, "root:Postgres");
        assert_ne!(root_pg, hmac16_with(master, "Postgres:proj:app:production"));
        assert_ne!(root_pg, hmac16_with(master, "root:Mariadb"));
        assert_ne!(
            root_pg,
            hmac16_with(b"a-different-master-key-32-bytes!", "root:Postgres")
        );
        assert_eq!(root_pg.len(), 32); // 16 bytes hex
    }

    #[test]
    fn restore_script_targets_the_app_db_and_rejects_unsupported() {
        use super::restore_script;
        use majnet_common::manifest::DbEngine;
        assert!(restore_script(DbEngine::Postgres, "proj_app_production")
            .unwrap()
            .contains(r#"-d "proj_app_production""#));
        assert!(restore_script(DbEngine::Mariadb, "d")
            .unwrap()
            .contains("mariadb -uroot"));
        assert!(restore_script(DbEngine::Mongodb, "d").is_err());
        assert!(restore_script(DbEngine::Valkey, "d").is_err());
    }

    #[test]
    fn db_names_are_identifier_safe() {
        assert_eq!(
            db_name("zpevnik", "api-pr12", EnvClass::Ephemeral),
            "zpevnik_api_pr12_ephemeral"
        );
        assert_eq!(
            db_name("zpevnik", "api", EnvClass::Production),
            "zpevnik_api_production"
        );
    }
}
