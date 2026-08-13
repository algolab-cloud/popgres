//! Starting, adopting and stopping the Postgres process itself.

use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use postgresql_embedded::{PostgreSQL, Settings, SettingsBuilder, VersionReq};

use crate::project::Project;
use crate::seed;
use crate::state::InstanceState;
use crate::ENV_VAR;

const DEFAULT_DATABASE: &str = "db";
const LOCALHOST: &str = "127.0.0.1";

pub struct Started {
    pub state: InstanceState,
    /// We found it already running and adopted it, rather than starting it.
    pub already_running: bool,
}

pub fn port_is_open(port: u16) -> bool {
    TcpStream::connect_timeout(
        &(std::net::Ipv4Addr::LOCALHOST, port).into(),
        Duration::from_millis(300),
    )
    .is_ok()
}

/// Settings shared by every instance of a given project, before we know
/// whether we're starting fresh or resuming a kept data directory.
fn base_settings(state_dir: &Path) -> SettingsBuilder {
    SettingsBuilder::new()
        .data_dir(state_dir.join("data"))
        .password_file(state_dir.join(".pgpass"))
        .host(LOCALHOST)
        // We own the lifecycle; the crate must not clean up behind us.
        .temporary(false)
}

/// Rebuild the settings for an instance we started earlier, so we can talk to it.
fn settings_for(state_dir: &Path, state: &InstanceState) -> Settings {
    base_settings(state_dir)
        .version(VersionReq::from_str(&state.pg_version).unwrap_or_default())
        .installation_dir(PathBuf::from(&state.installation_dir))
        .data_dir(PathBuf::from(&state.data_dir))
        .host(state.host.clone())
        .port(state.port)
        .username(state.username.clone())
        .password(state.password.clone())
        .build()
}

/// Start (or adopt) this project's instance and persist its state.
///
/// Flags win over `popgres.toml`, which wins over the built-in defaults.
pub async fn start(
    project: &Project,
    keep: bool,
    port: Option<u16>,
    pg: Option<String>,
) -> Result<Started> {
    let state_dir = project.state_dir.as_path();
    let keep = keep || project.config.keep.unwrap_or(false);
    let port = port.or_else(|| project.config.fixed_port());
    let pg = pg.or_else(|| project.config.pg_version.clone());

    let previous = InstanceState::load(state_dir)?;

    // Already running? Be idempotent.
    if let Some(existing) = previous.as_ref() {
        if port_is_open(existing.port) {
            project.write_env_file(&existing.url())?;
            return Ok(Started {
                state: existing.clone(),
                already_running: true,
            });
        }
    }

    // initdb writes the password file into the state dir, so it has to exist first.
    std::fs::create_dir_all(state_dir)
        .with_context(|| format!("cannot create {}", state_dir.display()))?;

    let mut builder = base_settings(state_dir);
    let data_dir = state_dir.join("data");
    // An existing database keeps the name it was created with; renaming it in
    // config would otherwise silently strand the data.
    let database = previous.as_ref().map_or_else(
        || {
            project
                .config
                .database
                .clone()
                .unwrap_or_else(|| DEFAULT_DATABASE.to_string())
        },
        |p| p.database.clone(),
    );

    // A data dir left behind by `down --keep` was initdb'd with its own
    // credentials. Reuse them, or nothing we hand out will authenticate.
    let resuming = data_dir.join("postgresql.conf").exists();
    if resuming {
        let Some(previous) = previous.as_ref() else {
            bail!(
                "{} holds an initialized database but its state file is gone, so its \
                 password cannot be recovered — delete {} and start over",
                data_dir.display(),
                state_dir.display()
            );
        };
        builder = builder
            .username(previous.username.clone())
            .password(previous.password.clone())
            .installation_dir(PathBuf::from(&previous.installation_dir))
            .version(VersionReq::from_str(&previous.pg_version).unwrap_or_default());
    }

    // Only a fresh instance can take a configured password: an existing role
    // already has whatever it was created with.
    let configured_password = if resuming {
        None
    } else {
        project.config.password.clone()
    };
    if let Some(password) = configured_password.as_ref() {
        builder = builder.password(password.clone());
    }

    if let Some(pg) = pg.as_deref() {
        let version = VersionReq::from_str(pg)
            .with_context(|| format!("invalid Postgres version requirement: {pg}"))?;
        builder = builder.version(version);
    }
    if let Some(port) = port {
        builder = builder.port(port);
    }

    let mut postgresql = PostgreSQL::new(builder.build());
    if !resuming {
        eprintln!(
            "popgres: initializing a fresh database (the first run of a Postgres version downloads it)..."
        );
    }
    postgresql
        .setup()
        .await
        .context("failed to set up Postgres")?;

    // With no password configured, drop the auth requirement entirely — the
    // server only listens on loopback, and a URL with no secret in it is far
    // easier to paste, log, and hand to an agent.
    let passwordless = !resuming && configured_password.is_none();
    if passwordless {
        write_trust_hba(&data_dir)?;
    }

    postgresql
        .start()
        .await
        .context("failed to start Postgres")?;

    if !postgresql
        .database_exists(&database)
        .await
        .context("failed to check database")?
    {
        postgresql
            .create_database(&database)
            .await
            .context("failed to create database")?;
    }

    let settings = postgresql.settings();
    let state = InstanceState {
        project_dir: project.root.display().to_string(),
        data_dir: settings.data_dir.display().to_string(),
        installation_dir: settings.installation_dir.display().to_string(),
        host: settings.host.clone(),
        port: settings.port,
        username: settings.username.clone(),
        // Under trust auth the role's password is never checked, so we record
        // none — that is what keeps it out of every URL we hand out.
        password: if passwordless {
            String::new()
        } else {
            settings.password.clone()
        },
        database,
        // `setup` resolves the requirement to a concrete install, and the
        // directory it picked is named for the version we actually got —
        // `settings.version` may still be the bare requirement (`*`).
        pg_version: settings.installation_dir.file_name().map_or_else(
            || settings.version.to_string(),
            |name| name.to_string_lossy().into_owned(),
        ),
        keep,
    };
    state.save(state_dir)?;

    // `PostgreSQL`'s `Drop` shuts the server down. That is exactly wrong here:
    // the whole point is that it outlives this process until someone stops it.
    std::mem::forget(postgresql);

    // Seeding is what makes wiped-by-default pleasant, so it runs on every
    // fresh database — but never over data we just resumed.
    if !resuming {
        if let Some(recipe) = project.config.seed.as_deref() {
            seed::run(project, &state, recipe)?;
        }
    }
    project.write_env_file(&state.url())?;

    Ok(Started {
        state,
        already_running: false,
    })
}

/// Stop the instance and, unless we're keeping it, wipe everything it left behind.
pub async fn stop(project: &Project, state: &InstanceState, keep: bool) -> Result<()> {
    let state_dir = project.state_dir.as_path();
    if port_is_open(state.port) {
        PostgreSQL::new(settings_for(state_dir, state))
            .stop()
            .await
            .context("failed to stop Postgres")?;
    }
    if !keep {
        std::fs::remove_dir_all(state_dir)
            .with_context(|| format!("failed to wipe {}", state_dir.display()))?;
    }
    // The URL is dead either way: a kept instance comes back on a new port.
    project.clear_env_file()
}

/// Replace the freshly-initdb'd `pg_hba.conf` with one that asks for no password.
///
/// The server is bound to loopback and lives and dies with the project, so the
/// password buys little; dropping it means `DATABASE_URL` holds no secret.
/// Anyone with an account on this machine can reach the instance — set
/// `password` in popgres.toml if that matters to you.
fn write_trust_hba(data_dir: &Path) -> Result<()> {
    let hba = data_dir.join("pg_hba.conf");
    let contents = "\
# Written by popgres. This instance listens on loopback only and takes no
# password. Set `password` in popgres.toml to require one instead.
local   all   all                   trust
host    all   all   127.0.0.1/32    trust
host    all   all   ::1/128         trust
";
    std::fs::write(&hba, contents).with_context(|| format!("cannot write {}", hba.display()))
}

/// Environment every child of popgres gets, so apps and `psql` alike just work.
pub fn instance_env(state: &InstanceState) -> Vec<(&'static str, String)> {
    let mut env = vec![
        (ENV_VAR, state.url()),
        ("POPGRES_URL", state.url()),
        ("PGHOST", state.host.clone()),
        ("PGPORT", state.port.to_string()),
        ("PGUSER", state.username.clone()),
        ("PGDATABASE", state.database.clone()),
    ];
    // An empty PGPASSWORD is not the same as an unset one for some clients.
    if !state.password.is_empty() {
        env.push(("PGPASSWORD", state.password.clone()));
    }
    env
}

/// The `psql` shipped alongside the cached server binaries.
pub fn psql_binary(state: &InstanceState) -> Result<PathBuf> {
    let binary = PathBuf::from(&state.installation_dir)
        .join("bin")
        .join(if cfg!(windows) { "psql.exe" } else { "psql" });
    if !binary.exists() {
        bail!(
            "psql is missing from {} — the cached PostgreSQL install looks incomplete",
            binary.display()
        );
    }
    Ok(binary)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> InstanceState {
        InstanceState {
            project_dir: "/tmp/project".to_string(),
            data_dir: "/tmp/state/data".to_string(),
            installation_dir: "/tmp/install/18.4.0".to_string(),
            host: "127.0.0.1".to_string(),
            port: 54329,
            username: "postgres".to_string(),
            password: String::new(),
            database: "db".to_string(),
            pg_version: "18.4.0".to_string(),
            keep: false,
        }
    }

    #[test]
    fn a_port_nothing_is_listening_on_reads_as_closed() {
        // Port 1 is privileged and unused; nothing local should answer there.
        assert!(!port_is_open(1));
    }

    #[test]
    fn passwordless_instances_leave_pgpassword_unset() {
        let env = instance_env(&sample());
        assert!(!env.iter().any(|(key, _)| *key == "PGPASSWORD"));
        assert_eq!(
            env.iter().find(|(key, _)| *key == ENV_VAR).unwrap().1,
            "postgresql://postgres@127.0.0.1:54329/db"
        );
    }

    #[test]
    fn a_password_reaches_children_through_pgpassword() {
        let state = InstanceState {
            password: "hunter2".to_string(),
            ..sample()
        };
        let env = instance_env(&state);
        assert_eq!(
            env.iter().find(|(key, _)| *key == "PGPASSWORD").unwrap().1,
            "hunter2"
        );
    }

    #[test]
    fn the_trust_file_grants_loopback_only() {
        let dir = tempfile::tempdir().unwrap();
        write_trust_hba(dir.path()).unwrap();

        let hba = std::fs::read_to_string(dir.path().join("pg_hba.conf")).unwrap();
        assert!(hba.contains("host    all   all   127.0.0.1/32    trust"));
        // Nothing outside loopback should be mentioned at all.
        assert!(!hba.contains("0.0.0.0"));
    }
}
