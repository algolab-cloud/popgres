//! popgres — disposable Postgres for every project.
//!
//! Pops up when you start, pops away when you stop.
//! No system install, no Docker: real PostgreSQL binaries are fetched once,
//! cached globally, and run as a plain local process.

mod config;
mod state;

use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use postgresql_embedded::{PostgreSQL, Settings, SettingsBuilder, VersionReq};

use config::Config;
use state::{project_state_dir, InstanceState};

const DEFAULT_DATABASE: &str = "db";
const LOCALHOST: &str = "127.0.0.1";

/// The variable everything downstream reads: children, env files, agents.
const ENV_VAR: &str = "DATABASE_URL";

/// How long a child gets to wind down on its own after Ctrl-C before we insist.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

#[derive(Parser)]
#[command(
    name = "popgres",
    version,
    about = "Disposable Postgres for every project — pops up when you start, pops away when you stop.",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start a Postgres instance for this project (downloads binaries on first ever run)
    Up {
        /// Keep the data directory when the instance is later stopped
        #[arg(long)]
        keep: bool,
        /// Fixed port to listen on (default: pick a free port)
        #[arg(long)]
        port: Option<u16>,
        /// Postgres version requirement, e.g. "=18.4.0" or "18" (default: latest stable)
        #[arg(long)]
        pg: Option<String>,
        /// Emit machine-readable JSON on stdout
        #[arg(long)]
        json: bool,
    },
    /// Run a command with DATABASE_URL set, then dispose of the database
    ///
    /// The instance lives exactly as long as the command: `popgres run -- npm run dev`.
    /// Ctrl-C tears down both. An instance that was already up is reused and left running.
    Run {
        /// Keep the data directory instead of wiping it when the command exits
        #[arg(long)]
        keep: bool,
        /// Fixed port to listen on (default: pick a free port)
        #[arg(long)]
        port: Option<u16>,
        /// Postgres version requirement, e.g. "=18.4.0" or "18" (default: latest stable)
        #[arg(long)]
        pg: Option<String>,
        /// The command to run, after `--`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        cmd: Vec<String>,
    },
    /// Stop the instance; wipes its data unless --keep (or it was started with --keep)
    Down {
        /// Keep the data directory for the next `up`
        #[arg(long)]
        keep: bool,
        /// Wipe the data even if the instance or popgres.toml asked to keep it
        #[arg(long, conflicts_with = "keep")]
        wipe: bool,
        /// Emit machine-readable JSON on stdout
        #[arg(long)]
        json: bool,
    },
    /// Print the connection URL of the running instance
    Url,
    /// Open a psql shell into the running instance
    Psql {
        /// Extra arguments passed straight through to psql, after `--`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Wipe the data and start again from a fresh database, even if it was kept
    Reset {
        /// Emit machine-readable JSON on stdout
        #[arg(long)]
        json: bool,
    },
    /// Show whether this project's instance is running
    Status {
        /// Emit machine-readable JSON on stdout
        #[arg(long)]
        json: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Up {
            keep,
            port,
            pg,
            json,
        } => up(keep, port, pg, json).await,
        Command::Run {
            keep,
            port,
            pg,
            cmd,
        } => run(keep, port, pg, cmd).await,
        Command::Down { keep, wipe, json } => down(keep, wipe, json).await,
        Command::Url => url(),
        Command::Psql { args } => psql(args),
        Command::Reset { json } => reset(json).await,
        Command::Status { json } => status(json),
    }
}

/// The project the current directory belongs to, its optional `popgres.toml`,
/// and where its instance lives.
struct Project {
    root: PathBuf,
    state_dir: PathBuf,
    config: Config,
}

impl Project {
    fn discover() -> Result<Self> {
        let cwd = std::env::current_dir()
            .context("cannot determine current directory")?
            .canonicalize()
            .context("cannot canonicalize current directory")?;
        let (root, config) = config::discover(&cwd)?;
        let state_dir = project_state_dir(&root)?;
        Ok(Self {
            root,
            state_dir,
            config,
        })
    }

    fn state(&self) -> Result<Option<InstanceState>> {
        InstanceState::load(&self.state_dir)
    }

    /// The state of a running instance, or a clear error explaining there isn't one.
    fn running_instance(&self) -> Result<InstanceState> {
        let Some(state) = self.state()? else {
            bail!("no popgres instance found for this project — run `popgres up` first");
        };
        if !port_is_open(state.port) {
            bail!("instance is not running — run `popgres up`");
        }
        Ok(state)
    }

    /// The configured `env_file`, resolved against the project root.
    fn env_file(&self) -> Option<PathBuf> {
        self.config
            .env_file
            .as_ref()
            .map(|path| self.root.join(path))
    }

    /// Point the project's `env_file` at this instance, leaving any other
    /// variables in the file alone.
    fn write_env_file(&self, url: &str) -> Result<()> {
        let Some(path) = self.env_file() else {
            return Ok(());
        };
        let mut lines = env_file_lines_without_url(&path)?;
        lines.push(format!("{ENV_VAR}={url}"));
        write_private(&path, &lines.join("\n"))
            .with_context(|| format!("cannot write {}", path.display()))
    }

    /// Drop the dead URL from the `env_file` once the instance is gone.
    fn clear_env_file(&self) -> Result<()> {
        let Some(path) = self.env_file() else {
            return Ok(());
        };
        if !path.exists() {
            return Ok(());
        }
        let lines = env_file_lines_without_url(&path)?;
        if lines.is_empty() {
            // We created it and it holds nothing else — take it with us.
            std::fs::remove_file(&path)
                .with_context(|| format!("cannot remove {}", path.display()))?;
            return Ok(());
        }
        write_private(&path, &lines.join("\n"))
            .with_context(|| format!("cannot write {}", path.display()))
    }
}

/// The existing contents of an env file, minus any line that sets our variable.
fn env_file_lines_without_url(path: &Path) -> Result<Vec<String>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    Ok(raw
        .lines()
        .filter(|line| !line.trim_start().starts_with(&format!("{ENV_VAR}=")))
        .map(str::to_string)
        .collect())
}

/// Write a file only its owner can read — the URL in it contains the password.
fn write_private(path: &Path, contents: &str) -> Result<()> {
    let body = format!("{contents}\n");
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(body.as_bytes())?;
    }
    #[cfg(not(unix))]
    std::fs::write(path, body)?;
    Ok(())
}

fn port_is_open(port: u16) -> bool {
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

struct Started {
    state: InstanceState,
    /// We found it already running and adopted it, rather than starting it.
    already_running: bool,
}

/// Start (or adopt) this project's instance and persist its state.
///
/// Flags win over `popgres.toml`, which wins over the built-in defaults.
async fn start_instance(
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
        if let Some(seed) = project.config.seed.as_deref() {
            run_seed(project, &state, seed)?;
        }
    }
    project.write_env_file(&state.url())?;

    Ok(Started {
        state,
        already_running: false,
    })
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
fn instance_env(state: &InstanceState) -> Vec<(&'static str, String)> {
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

/// Run the configured `seed` against a freshly created database: a `.sql` file
/// goes through psql, anything else runs as a shell command with DATABASE_URL set.
fn run_seed(project: &Project, state: &InstanceState, seed: &str) -> Result<()> {
    let path = project.root.join(seed);
    let is_sql_file = path.is_file()
        && path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("sql"));

    eprintln!("popgres: seeding from {seed}");
    let status = if is_sql_file {
        std::process::Command::new(psql_binary(state)?)
            .arg(state.url())
            .args(["--quiet", "--no-psqlrc", "-v", "ON_ERROR_STOP=1", "-f"])
            .arg(&path)
            .current_dir(&project.root)
            .status()
            .with_context(|| format!("failed to run the seed file {seed}"))?
    } else {
        let (shell, flag) = if cfg!(windows) {
            ("cmd", "/C")
        } else {
            ("sh", "-c")
        };
        std::process::Command::new(shell)
            .arg(flag)
            .arg(seed)
            .envs(instance_env(state))
            .current_dir(&project.root)
            .status()
            .with_context(|| format!("failed to run the seed command `{seed}`"))?
    };

    if !status.success() {
        bail!("seed `{seed}` failed ({status}) — the database is up but not seeded");
    }
    Ok(())
}

/// The `psql` shipped alongside the cached server binaries.
fn psql_binary(state: &InstanceState) -> Result<PathBuf> {
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

/// Stop the instance and, unless we're keeping it, wipe everything it left behind.
async fn stop_instance(project: &Project, state: &InstanceState, keep: bool) -> Result<()> {
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

async fn up(keep: bool, port: Option<u16>, pg: Option<String>, json: bool) -> Result<()> {
    let project = Project::discover()?;
    let started = start_instance(&project, keep, port, pg).await?;
    emit_up(&started.state, json, started.already_running);
    Ok(())
}

fn emit_up(state: &InstanceState, json: bool, already: bool) {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "url": state.url(),
                "host": state.host,
                "port": state.port,
                "database": state.database,
                "already_running": already,
            })
        );
    } else {
        if already {
            eprintln!("popgres: already running on port {}", state.port);
        } else {
            eprintln!("popgres: up on port {}", state.port);
        }
        println!("{}", state.url());
    }
}

async fn run(keep: bool, port: Option<u16>, pg: Option<String>, cmd: Vec<String>) -> Result<()> {
    let project = Project::discover()?;
    let Started {
        state,
        already_running,
    } = start_instance(&project, keep, port, pg).await?;

    if already_running {
        eprintln!(
            "popgres: reusing the instance on port {} — it will be left running",
            state.port
        );
    } else {
        eprintln!("popgres: up on port {} — DATABASE_URL is set", state.port);
    }

    // The child owns stdout: everything popgres says here goes to stderr.
    let mut child = tokio::process::Command::new(&cmd[0])
        .args(&cmd[1..])
        .envs(instance_env(&state))
        .spawn()
        .with_context(|| format!("failed to run `{}`", cmd.join(" ")))?;

    let status = tokio::select! {
        result = child.wait() => result.context("failed to wait for the child process")?,
        () = shutdown_signal() => {
            // Ctrl-C reaches the whole foreground process group, so the child is
            // usually already on its way out — let it finish before insisting.
            match tokio::time::timeout(SHUTDOWN_GRACE, child.wait()).await {
                Ok(result) => result.context("failed to wait for the child process")?,
                Err(_) => {
                    eprintln!("popgres: command did not exit, killing it");
                    child.start_kill().ok();
                    child.wait().await.context("failed to wait for the child process")?
                }
            }
        }
    };

    // Teardown runs however the child exited. `state.keep` is the resolved
    // answer, since `keep = true` can also come from popgres.toml.
    if already_running {
        eprintln!("popgres: leaving the instance that was already running");
    } else {
        stop_instance(&project, &state, state.keep).await?;
        if state.keep {
            eprintln!("popgres: stopped (data kept)");
        } else {
            eprintln!("popgres: stopped and wiped — poof!");
        }
    }

    std::process::exit(status.code().unwrap_or(1));
}

/// Resolves when the user (or the OS) asks us to shut down.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

async fn down(keep_flag: bool, wipe_flag: bool, json: bool) -> Result<()> {
    let project = Project::discover()?;
    let Some(state) = project.state()? else {
        bail!("no popgres instance found for this project (nothing to stop)");
    };

    // Without --wipe there is no way out of a `keep = true` config.
    let keep = (keep_flag || state.keep) && !wipe_flag;
    stop_instance(&project, &state, keep).await?;

    if json {
        println!("{}", serde_json::json!({ "stopped": true, "wiped": !keep }));
    } else if keep {
        eprintln!("popgres: stopped (data kept — next `up` resumes)");
    } else {
        eprintln!("popgres: stopped and wiped — poof!");
    }
    Ok(())
}

fn url() -> Result<()> {
    println!("{}", Project::discover()?.running_instance()?.url());
    Ok(())
}

fn psql(args: Vec<String>) -> Result<()> {
    let state = Project::discover()?.running_instance()?;
    let binary = psql_binary(&state)?;
    let status = std::process::Command::new(&binary)
        .arg(state.url())
        .args(&args)
        .status()
        .with_context(|| format!("failed to run {}", binary.display()))?;
    std::process::exit(status.code().unwrap_or(1));
}

async fn reset(json: bool) -> Result<()> {
    let project = Project::discover()?;
    let Some(state) = project.state()? else {
        bail!("no popgres instance found for this project — run `popgres up` first");
    };

    // Reset means fresh, so the data goes even for a --keep instance. Holding on
    // to the port keeps the URL stable for anything already pointed at it.
    stop_instance(&project, &state, false).await?;
    let started = start_instance(
        &project,
        state.keep,
        Some(state.port),
        Some(state.pg_version.clone()),
    )
    .await?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "reset": true,
                "url": started.state.url(),
                "port": started.state.port,
                "database": started.state.database,
            })
        );
    } else {
        eprintln!(
            "popgres: wiped and re-initialized on port {}",
            started.state.port
        );
        println!("{}", started.state.url());
    }
    Ok(())
}

fn status(json: bool) -> Result<()> {
    let project = Project::discover()?;
    let state = project.state()?;
    let running = state.as_ref().is_some_and(|s| port_is_open(s.port));

    if json {
        println!(
            "{}",
            serde_json::json!({
                "running": running,
                "port": state.as_ref().map(|s| s.port),
                "pg_version": state.as_ref().map(|s| s.pg_version.clone()),
                "keep": state.as_ref().map(|s| s.keep),
            })
        );
    } else if let Some(s) = state {
        if running {
            println!(
                "running — Postgres {} on port {}",
                s.pg_version.trim_start_matches('='),
                s.port
            );
        } else {
            println!(
                "stopped (state present{})",
                if s.keep { ", data kept" } else { "" }
            );
        }
    } else {
        println!("no instance for this project");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Command {
        Cli::try_parse_from(args).expect("should parse").command
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn run_takes_everything_after_the_separator_as_the_command() {
        let Command::Run { cmd, keep, .. } = parse(&["popgres", "run", "--", "npm", "run", "dev"])
        else {
            panic!("expected a run command");
        };
        assert_eq!(cmd, ["npm", "run", "dev"]);
        assert!(!keep);
    }

    #[test]
    fn run_flags_before_the_separator_belong_to_popgres() {
        let Command::Run { cmd, keep, port, .. } =
            parse(&["popgres", "run", "--keep", "--port", "5555", "--", "vite", "dev"])
        else {
            panic!("expected a run command");
        };
        assert!(keep);
        assert_eq!(port, Some(5555));
        assert_eq!(cmd, ["vite", "dev"]);
    }

    #[test]
    fn run_passes_the_childs_own_flags_through_untouched() {
        // The child's `--keep` is the child's business, not ours.
        let Command::Run { cmd, keep, .. } =
            parse(&["popgres", "run", "--", "cargo", "test", "--keep", "--nocapture"])
        else {
            panic!("expected a run command");
        };
        assert!(!keep);
        assert_eq!(cmd, ["cargo", "test", "--keep", "--nocapture"]);
    }

    #[test]
    fn run_requires_something_to_run() {
        assert!(Cli::try_parse_from(["popgres", "run"]).is_err());
    }

    #[test]
    fn psql_forwards_extra_arguments() {
        let Command::Psql { args } = parse(&["popgres", "psql", "--", "-c", "select 1"]) else {
            panic!("expected a psql command");
        };
        assert_eq!(args, ["-c", "select 1"]);
    }

    #[test]
    fn down_cannot_both_keep_and_wipe() {
        assert!(Cli::try_parse_from(["popgres", "down", "--keep", "--wipe"]).is_err());
    }

    #[test]
    fn psql_needs_no_arguments() {
        let Command::Psql { args } = parse(&["popgres", "psql"]) else {
            panic!("expected a psql command");
        };
        assert!(args.is_empty());
    }

    #[test]
    fn a_port_nothing_is_listening_on_reads_as_closed() {
        // Port 1 is privileged and unused; nothing local should answer there.
        assert!(!port_is_open(1));
    }
}
