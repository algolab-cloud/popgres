//! popgres — disposable Postgres for every project.
//!
//! Pops up when you start, pops away when you stop.
//! No system install, no Docker: real PostgreSQL binaries are fetched once,
//! cached globally, and run as a plain local process.

mod cache;
mod commands;
mod config;
mod error;
mod extensions;
mod instance;
mod project;
mod registry;
mod seed;
mod state;

use clap::{Parser, Subcommand};

/// The variable everything downstream reads: children, env files, agents.
const ENV_VAR: &str = "DATABASE_URL";

#[derive(Parser)]
#[command(
    name = "popgres",
    version,
    about = "Disposable Postgres for every project — pops up when you start, pops away when you stop.",
    long_about = None
)]
struct Cli {
    /// Emit machine-readable JSON: results on stdout, lifecycle events and errors on stderr
    #[arg(long, global = true)]
    json: bool,
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
        /// Dispose of this instance after a deadline, e.g. "30m" (reaped by `popgres gc`)
        #[arg(long)]
        ttl: Option<String>,
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
        /// Deadline for the instance if the command leaves it running, e.g. "30m"
        #[arg(long)]
        ttl: Option<String>,
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
    Reset,
    /// Show whether this project's instance is running
    Status,
    /// List every instance on this machine, in any project
    List,
    /// Dispose of instances past their --ttl, in every project on this machine
    Gc {
        /// Report what would be disposed of without stopping or wiping anything
        #[arg(long)]
        dry_run: bool,
    },
    /// Show popgres's disk usage: PostgreSQL versions, extension variants, instances
    Cache {
        /// Remove unused extension variants
        #[arg(long)]
        clean: bool,
        /// With --clean, also remove PostgreSQL versions no popgres instance
        /// uses (the download cache may be shared with other tools)
        #[arg(long, requires = "clean")]
        all: bool,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let json = cli.json;
    if let Err(failure) = dispatch(cli.command, json).await {
        let exit_code = error::exit_code(&failure);
        if json {
            eprintln!(
                "{}",
                serde_json::json!({
                    "ok": false,
                    // {:#} keeps the whole cause chain — agents get no less
                    // diagnostic detail than the human-readable path below.
                    "error": { "message": format!("{failure:#}") },
                    "exit_code": exit_code,
                })
            );
        } else {
            eprintln!("Error: {failure:#}");
        }
        std::process::exit(exit_code);
    }
}

async fn dispatch(command: Command, json: bool) -> anyhow::Result<()> {
    match command {
        Command::Up {
            keep,
            port,
            pg,
            ttl,
        } => {
            let already_running = commands::up(keep, port, pg, ttl, json).await?;
            if already_running {
                std::process::exit(error::ALREADY_RUNNING);
            }
            Ok(())
        }
        Command::Run {
            keep,
            port,
            pg,
            ttl,
            cmd,
        } => commands::run(keep, port, pg, ttl, json, cmd).await,
        Command::Down { keep, wipe } => commands::down(keep, wipe, json).await,
        Command::Url => commands::url(json),
        Command::Psql { args } => commands::psql(args),
        Command::Reset => commands::reset(json).await,
        Command::Status => {
            if !commands::status(json)? {
                std::process::exit(error::NOT_RUNNING);
            }
            Ok(())
        }
        Command::List => commands::list(json),
        Command::Gc { dry_run } => commands::gc(dry_run, json).await,
        Command::Cache { clean, all } => commands::cache(clean, all, json),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).expect("should parse")
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn run_takes_everything_after_the_separator_as_the_command() {
        let Command::Run { cmd, keep, .. } =
            parse(&["popgres", "run", "--", "npm", "run", "dev"]).command
        else {
            panic!("expected a run command");
        };
        assert_eq!(cmd, ["npm", "run", "dev"]);
        assert!(!keep);
    }

    #[test]
    fn run_flags_before_the_separator_belong_to_popgres() {
        let Command::Run {
            cmd,
            keep,
            port,
            ttl,
            ..
        } = parse(&[
            "popgres", "run", "--keep", "--port", "5555", "--ttl", "30m", "--", "vite", "dev",
        ])
        .command
        else {
            panic!("expected a run command");
        };
        assert!(keep);
        assert_eq!(port, Some(5555));
        assert_eq!(ttl.as_deref(), Some("30m"));
        assert_eq!(cmd, ["vite", "dev"]);
    }

    #[test]
    fn run_passes_the_childs_own_flags_through_untouched() {
        // The child's `--keep` and `--json` are the child's business, not ours.
        let cli = parse(&["popgres", "run", "--", "cargo", "test", "--keep", "--json"]);
        assert!(!cli.json);
        let Command::Run { cmd, keep, .. } = cli.command else {
            panic!("expected a run command");
        };
        assert!(!keep);
        assert_eq!(cmd, ["cargo", "test", "--keep", "--json"]);
    }

    #[test]
    fn run_requires_something_to_run() {
        assert!(Cli::try_parse_from(["popgres", "run"]).is_err());
    }

    #[test]
    fn psql_forwards_extra_arguments() {
        let Command::Psql { args } = parse(&["popgres", "psql", "--", "-c", "select 1"]).command
        else {
            panic!("expected a psql command");
        };
        assert_eq!(args, ["-c", "select 1"]);
    }

    #[test]
    fn psql_needs_no_arguments() {
        let Command::Psql { args } = parse(&["popgres", "psql"]).command else {
            panic!("expected a psql command");
        };
        assert!(args.is_empty());
    }

    #[test]
    fn down_cannot_both_keep_and_wipe() {
        assert!(Cli::try_parse_from(["popgres", "down", "--keep", "--wipe"]).is_err());
    }

    #[test]
    fn list_is_a_global_read_only_command() {
        assert!(matches!(parse(&["popgres", "list"]).command, Command::List));
        assert!(parse(&["popgres", "list", "--json"]).json);
    }

    #[test]
    fn gc_is_a_global_cleanup_command() {
        assert!(matches!(
            parse(&["popgres", "gc"]).command,
            Command::Gc { dry_run: false }
        ));
        assert!(matches!(
            parse(&["popgres", "gc", "--dry-run"]).command,
            Command::Gc { dry_run: true }
        ));
    }

    #[test]
    fn json_is_one_global_flag_valid_on_every_subcommand() {
        assert!(parse(&["popgres", "run", "--json", "--", "true"]).json);
        assert!(parse(&["popgres", "url", "--json"]).json);
        assert!(parse(&["popgres", "psql", "--json"]).json);
        assert!(parse(&["popgres", "status", "--json"]).json);
        assert!(!parse(&["popgres", "status"]).json);
    }
}
