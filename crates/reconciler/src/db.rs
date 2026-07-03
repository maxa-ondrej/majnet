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
//! Engine containers (`majnet-postgres`, `majnet-mariadb`) are platform
//! services (§10 `platform/` manifests) — provisioning execs into them and
//! attaches them to the project's network so the app can resolve them by
//! container name.

use anyhow::{bail, Context, Result};
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
        DbEngine::Valkey | DbEngine::Mongodb => {
            bail!(
                "engine {engine:?} provisioning is not implemented yet (roadmap phase 5 remainder)"
            )
        }
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
            r#"mariadb -uroot -p"$MARIADB_ROOT_PASSWORD" -e "CREATE DATABASE IF NOT EXISTS \`{name}\`; CREATE USER IF NOT EXISTS '{name}'@'%' IDENTIFIED BY '{password}'; ALTER USER '{name}'@'%' IDENTIFIED BY '{password}'; GRANT ALL ON \`{name}\`.* TO '{name}'@'%';""#
        ),
        _ => unreachable!(),
    };
    exec(docker, container, &script)
        .await
        .with_context(|| format!("provisioning {name} on {container}"))?;
    tracing::debug!(project, app, db = name, "database ensured");
    Ok(env)
}

/// `<project>_<app>_<class>` with `-` → `_` (identifier-safe, ≤63 chars for pg).
fn db_name(project: &str, app: &str, class: EnvClass) -> String {
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
    let key_path = config.age_key_dir.join("db-master.key");
    let master = std::fs::read(&key_path).with_context(|| {
        format!(
            "missing DB master key {} (generate: openssl rand -hex 32 > …)",
            key_path.display()
        )
    })?;
    let mut mac = Hmac::<Sha256>::new_from_slice(&master).expect("any key length");
    mac.update(format!("{engine:?}:{project}:{app}:{}", class.as_str()).as_bytes());
    Ok(hex::encode(&mac.finalize().into_bytes()[..16]))
}

fn engine_container(engine: DbEngine) -> &'static str {
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
    use super::db_name;
    use majnet_common::EnvClass;

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
