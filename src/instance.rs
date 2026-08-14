//! Starting, adopting and stopping the Postgres process itself.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use postgresql_embedded::{PostgreSQL, Settings, SettingsBuilder, Version, VersionReq};

use crate::commands::emit_event;
use crate::project::Project;
use crate::state::{wipe_state_dir, InstanceState, StateLock};
use crate::ENV_VAR;

const DEFAULT_DATABASE: &str = "db";
const LOCALHOST: &str = "127.0.0.1";

pub struct Started {
    pub state: InstanceState,
    /// We found it already running and adopted it, rather than starting it.
    pub already_running: bool,
    /// A brand-new initdb — the caller should run the seed hook.
    pub freshly_initialized: bool,
}

/// What we could establish about the recorded instance.
///
/// `Unverifiable` is the load-bearing variant: when the postmaster identity
/// matches but liveness cannot be confirmed either way, callers must fail
/// loudly rather than wipe or re-init a possibly-live data directory.
pub enum Liveness {
    Running,
    NotRunning,
    Unverifiable(String),
}

/// Confirm that the recorded data directory owns a live PostgreSQL process on
/// the recorded port. A bare TCP connection is insufficient because another
/// service may have claimed the port after a crash.
pub fn probe(state: &InstanceState) -> Liveness {
    let data_dir = Path::new(&state.data_dir);
    let pid_file = data_dir.join("postmaster.pid");
    let raw = match std::fs::read_to_string(&pid_file) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Liveness::NotRunning,
        Err(error) => {
            return Liveness::Unverifiable(format!("cannot read {}: {error}", pid_file.display()))
        }
    };
    if !postmaster_identity_matches(state, &raw) {
        return Liveness::NotRunning;
    }

    // From here on the pid file provably belongs to this instance, so an
    // inconclusive check must not be reported as "not running" — that is what
    // lets a wipe pull the data directory out from under a live postmaster.
    let pg_ctl = installation_binary(state, "pg_ctl");
    if !pg_ctl.exists() {
        return if postgres_handshake(&state.host, state.port) {
            Liveness::Running
        } else {
            Liveness::Unverifiable(format!(
                "the cached PostgreSQL install at {} is gone, so the recorded instance \
                 cannot be verified or stopped — if it is not running, delete the data \
                 directory {} yourself",
                state.installation_dir, state.data_dir,
            ))
        };
    }
    let status = std::process::Command::new(&pg_ctl)
        .args(["status", "-D"])
        .arg(data_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match status {
        Ok(status) if status.success() => {
            if postgres_handshake(&state.host, state.port) {
                Liveness::Running
            } else {
                Liveness::Unverifiable(format!(
                    "the postmaster (PID file {}) looks alive but did not answer a \
                     PostgreSQL handshake on port {} — it may still be starting up; \
                     retry in a moment",
                    pid_file.display(),
                    state.port,
                ))
            }
        }
        Ok(_) => Liveness::NotRunning,
        Err(error) => {
            Liveness::Unverifiable(format!("failed to run {}: {error}", pg_ctl.display()))
        }
    }
}

pub fn instance_is_running(state: &InstanceState) -> bool {
    matches!(probe(state), Liveness::Running)
}

fn postmaster_identity_matches(state: &InstanceState, pid_file: &str) -> bool {
    let lines: Vec<_> = pid_file.lines().collect();
    let Some(pid) = lines.first().and_then(|line| line.parse::<u32>().ok()) else {
        return false;
    };
    if pid == 0 || state.postmaster_pid.is_some_and(|expected| expected != pid) {
        return false;
    }
    let Some(data_dir) = lines.get(1) else {
        return false;
    };
    if !same_path(Path::new(data_dir), Path::new(&state.data_dir)) {
        return false;
    }
    lines
        .get(3)
        .and_then(|line| line.parse::<u16>().ok())
        .is_some_and(|port| port == state.port)
}

fn same_path(left: &Path, right: &Path) -> bool {
    left == right
        || left
            .canonicalize()
            .ok()
            .zip(right.canonicalize().ok())
            .is_some_and(|(left, right)| left == right)
}

/// Speak just enough of the PostgreSQL wire protocol to distinguish Postgres
/// from an arbitrary process listening on the same TCP port.
fn postgres_handshake(host: &str, port: u16) -> bool {
    let Ok(addresses) = (host, port).to_socket_addrs() else {
        return false;
    };
    for address in addresses {
        let Ok(mut stream) = TcpStream::connect_timeout(&address, Duration::from_millis(300)) else {
            continue;
        };
        stream
            .set_read_timeout(Some(Duration::from_millis(300)))
            .ok();
        stream
            .set_write_timeout(Some(Duration::from_millis(300)))
            .ok();
        // SSLRequest: int32 length (8), int32 request code (80877103), network order.
        if stream.write_all(&[0, 0, 0, 8, 4, 210, 22, 47]).is_err() {
            continue;
        }
        let mut response = [0_u8; 1];
        if stream.read_exact(&mut response).is_ok() && matches!(response[0], b'N' | b'S') {
            return true;
        }
    }
    false
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
/// Seeding is the caller's job: run the seed hook when `freshly_initialized`.
pub async fn start(
    project: &Project,
    keep: bool,
    port: Option<u16>,
    pg: Option<String>,
    json: bool,
) -> Result<Started> {
    let _lock = StateLock::acquire(&project.state_dir, json)?;
    start_locked(project, keep, port, pg, json).await
}

/// Stop the instance and, unless we're keeping it, wipe everything it left behind.
pub async fn stop(project: &Project, state: &InstanceState, keep: bool, json: bool) -> Result<()> {
    let _lock = StateLock::acquire(&project.state_dir, json)?;
    stop_locked(project, state, keep).await
}

/// Wipe and re-initialize under a single lock, so nothing can grab the port —
/// which reset deliberately holds on to — between the stop and the start.
pub async fn reset(project: &Project, state: &InstanceState, json: bool) -> Result<Started> {
    let _lock = StateLock::acquire(&project.state_dir, json)?;
    stop_locked(project, state, false).await?;
    start_locked(
        project,
        state.keep,
        Some(state.port),
        Some(state.pg_version.clone()),
        json,
    )
    .await
}

async fn start_locked(
    project: &Project,
    keep: bool,
    port: Option<u16>,
    pg: Option<String>,
    json: bool,
) -> Result<Started> {
    let state_dir = project.state_dir.as_path();
    let keep = keep || project.config.keep.unwrap_or(false);
    let port = port.or_else(|| project.config.fixed_port());
    let pg = pg.or_else(|| project.config.pg_version.clone());

    let previous = InstanceState::load(state_dir)?;

    // Already running? Be idempotent.
    if let Some(existing) = previous.as_ref() {
        match probe(existing) {
            Liveness::Running => {
                project.write_env_file(&existing.url())?;
                return Ok(Started {
                    state: existing.clone(),
                    already_running: true,
                    freshly_initialized: false,
                });
            }
            Liveness::NotRunning => {}
            Liveness::Unverifiable(reason) => {
                bail!("cannot tell whether this project's instance is still running: {reason}")
            }
        }
    }

    if let Some(port) = port {
        if tcp_port_is_bound(port) {
            return Err(crate::error::coded(
                crate::error::PORT_BUSY,
                format!("port {port} is already in use"),
            ));
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

        ensure_resume_version_matches(&data_dir, previous, pg.as_deref())?;
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
        emit_event(
            json,
            serde_json::json!({ "event": "initializing" }),
            "popgres: initializing a fresh database (the first run of a Postgres version downloads it)...",
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

    if let Err(start_error) = postgresql.start().await {
        // Our own postmaster may have come up anyway (e.g. the readiness wait
        // timed out) — stop it rather than orphan it behind the error. Only a
        // port that is still bound afterwards belongs to someone else.
        let _ = postgresql.stop().await;
        if port.is_some_and(tcp_port_is_bound) {
            return Err(crate::error::coded(
                crate::error::PORT_BUSY,
                format!("port {} is held by another process", port.unwrap()),
            ));
        }
        return Err(start_error).context("failed to start Postgres");
    }

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
    let postmaster_pid = read_postmaster_pid(&data_dir)?;
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
        postmaster_pid: Some(postmaster_pid),
        keep,
    };
    state.save(state_dir)?;

    // `PostgreSQL`'s `Drop` shuts the server down. That is exactly wrong here:
    // the whole point is that it outlives this process until someone stops it.
    std::mem::forget(postgresql);

    project.write_env_file(&state.url())?;

    Ok(Started {
        state,
        already_running: false,
        freshly_initialized: !resuming,
    })
}

async fn stop_locked(project: &Project, state: &InstanceState, keep: bool) -> Result<()> {
    let state_dir = project.state_dir.as_path();
    let Some(current) = InstanceState::load(state_dir)? else {
        return project.clear_env_file();
    };
    if current != *state {
        bail!("the popgres instance changed while waiting for the state lock — retry the command");
    }
    match probe(&current) {
        Liveness::Running => {
            PostgreSQL::new(settings_for(state_dir, &current))
                .stop()
                .await
                .context("failed to stop Postgres")?;
        }
        Liveness::NotRunning => {}
        // Never wipe a data directory we cannot prove is dead.
        Liveness::Unverifiable(reason) => {
            bail!("cannot tell whether the instance is still running: {reason}")
        }
    }
    if !keep {
        wipe_state_dir(state_dir)
            .with_context(|| format!("failed to wipe {}", state_dir.display()))?;
    }
    // The URL is dead either way: a kept instance comes back on a new port.
    project.clear_env_file()
}

fn read_postmaster_pid(data_dir: &Path) -> Result<u32> {
    let path = data_dir.join("postmaster.pid");
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    raw.lines()
        .next()
        .and_then(|line| line.parse::<u32>().ok())
        .filter(|pid| *pid != 0)
        .with_context(|| format!("invalid postmaster PID in {}", path.display()))
}

/// A kept data directory must agree with the saved state, and with the
/// requested version when there is one. The consistency half runs on every
/// resume — a mismatch otherwise surfaces as Postgres's raw "database files
/// are incompatible with server" long after the useful moment to say so.
fn ensure_resume_version_matches(
    data_dir: &Path,
    previous: &InstanceState,
    requested: Option<&str>,
) -> Result<()> {
    let version_file = data_dir.join("PG_VERSION");
    let data_major = std::fs::read_to_string(&version_file)
        .with_context(|| format!("cannot read {}", version_file.display()))?;
    let data_major = data_major.trim();
    let actual =
        Version::parse(previous.pg_version.trim_start_matches('=')).with_context(|| {
            format!(
                "invalid Postgres version in saved state: {}",
                previous.pg_version
            )
        })?;
    if actual.major.to_string() != data_major {
        bail!(
            "saved Postgres version {} does not match data directory version {}.x — run `popgres reset`",
            previous.pg_version,
            data_major
        );
    }
    let Some(requested) = requested else {
        return Ok(());
    };
    let requirement = VersionReq::from_str(requested)
        .with_context(|| format!("invalid Postgres version requirement: {requested}"))?;
    if !requirement.matches(&actual) {
        bail!(
            "data dir is PostgreSQL {}.x, but `{}` was requested — run `popgres reset` or pass `--pg {}`",
            data_major,
            requested,
            data_major
        );
    }
    Ok(())
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
    let binary = installation_binary(state, "psql");
    if !binary.exists() {
        bail!(
            "psql is missing from {} — the cached PostgreSQL install looks incomplete",
            binary.display()
        );
    }
    Ok(binary)
}

fn installation_binary(state: &InstanceState, name: &str) -> PathBuf {
    PathBuf::from(&state.installation_dir)
        .join("bin")
        .join(if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.to_string()
        })
}

fn tcp_port_is_bound(port: u16) -> bool {
    TcpStream::connect_timeout(
        &(std::net::Ipv4Addr::LOCALHOST, port).into(),
        Duration::from_millis(300),
    )
    .is_ok()
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
            postmaster_pid: Some(12345),
            keep: false,
        }
    }

    #[test]
    fn postmaster_identity_requires_the_recorded_data_dir_and_port() {
        let state = sample();
        let valid = format!(
            "12345\n{}\n1723456789\n{}\n/tmp\n127.0.0.1\n",
            state.data_dir, state.port
        );
        assert!(postmaster_identity_matches(&state, &valid));
        let wrong_pid = InstanceState {
            postmaster_pid: Some(54321),
            ..state.clone()
        };
        assert!(!postmaster_identity_matches(&wrong_pid, &valid));
        assert!(!postmaster_identity_matches(
            &state,
            &valid.replace(&state.port.to_string(), "54330")
        ));
        assert!(!postmaster_identity_matches(
            &state,
            &valid.replace(&state.data_dir, "/tmp/other/data")
        ));
    }

    #[test]
    fn a_matching_pid_file_without_binaries_is_unverifiable_not_stopped() {
        // The wipe-under-a-live-postmaster guard: identity matches, but the
        // cached install (and so pg_ctl) is gone and nothing answers the port.
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let state = InstanceState {
            data_dir: data_dir.display().to_string(),
            installation_dir: dir.path().join("no-such-install").display().to_string(),
            // An unused port so the handshake cannot succeed by accident.
            port: 1,
            ..sample()
        };
        std::fs::write(
            data_dir.join("postmaster.pid"),
            format!(
                "12345\n{}\n1723456789\n1\n/tmp\n127.0.0.1\n",
                state.data_dir
            ),
        )
        .unwrap();

        assert!(matches!(probe(&state), Liveness::Unverifiable(_)));
        assert!(!instance_is_running(&state));
    }

    #[test]
    fn a_missing_pid_file_reads_as_not_running() {
        let dir = tempfile::tempdir().unwrap();
        let state = InstanceState {
            data_dir: dir.path().display().to_string(),
            ..sample()
        };
        assert!(matches!(probe(&state), Liveness::NotRunning));
    }

    #[test]
    fn postgres_handshake_rejects_an_arbitrary_tcp_service() {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 8];
            stream.read_exact(&mut request).unwrap();
            stream.write_all(b"X").unwrap();
        });

        assert!(!postgres_handshake(LOCALHOST, port));
        server.join().unwrap();
    }

    #[test]
    fn postgres_handshake_accepts_a_postgres_ssl_response() {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 8];
            stream.read_exact(&mut request).unwrap();
            assert_eq!(request, [0, 0, 0, 8, 4, 210, 22, 47]);
            stream.write_all(b"N").unwrap();
        });

        assert!(postgres_handshake(LOCALHOST, port));
        server.join().unwrap();
    }

    #[test]
    fn a_listening_fixed_port_is_detected_as_busy() {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        assert!(tcp_port_is_bound(listener.local_addr().unwrap().port()));
    }

    #[test]
    fn resuming_rejects_a_different_postgres_major() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("PG_VERSION"), "18\n").unwrap();
        let error = ensure_resume_version_matches(dir.path(), &sample(), Some("19")).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("data dir is PostgreSQL 18.x"));
        assert!(message.contains("--pg 18"));
    }

    #[test]
    fn resuming_accepts_a_compatible_postgres_requirement() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("PG_VERSION"), "18\n").unwrap();
        ensure_resume_version_matches(dir.path(), &sample(), Some("18")).unwrap();
    }

    #[test]
    fn resuming_checks_state_against_the_data_dir_even_without_a_request() {
        // A plain `popgres up` must catch state/data disagreement too, not
        // only invocations that happen to pass --pg.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("PG_VERSION"), "17\n").unwrap();
        let error = ensure_resume_version_matches(dir.path(), &sample(), None).unwrap_err();
        assert!(error.to_string().contains("popgres reset"));
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
