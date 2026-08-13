//! popgres — disposable Postgres for every project.
//!
//! Pops up when you start, pops away when you stop.
//! No system install, no Docker: real PostgreSQL binaries are fetched once,
//! cached globally, and run as a plain local process.

mod commands;
mod config;
mod instance;
mod project;
mod seed;
mod state;

use anyhow::Result;
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
    match Cli::parse().command {
        Command::Up {
            keep,
            port,
            pg,
            json,
        } => commands::up(keep, port, pg, json).await,
        Command::Run {
            keep,
            port,
            pg,
            cmd,
        } => commands::run(keep, port, pg, cmd).await,
        Command::Down { keep, wipe, json } => commands::down(keep, wipe, json).await,
        Command::Url => commands::url(),
        Command::Psql { args } => commands::psql(args),
        Command::Reset { json } => commands::reset(json).await,
        Command::Status { json } => commands::status(json),
    }
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
        let Command::Run {
            cmd, keep, port, ..
        } = parse(&[
            "popgres", "run", "--keep", "--port", "5555", "--", "vite", "dev",
        ])
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
        let Command::Run { cmd, keep, .. } = parse(&[
            "popgres",
            "run",
            "--",
            "cargo",
            "test",
            "--keep",
            "--nocapture",
        ]) else {
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
    fn psql_needs_no_arguments() {
        let Command::Psql { args } = parse(&["popgres", "psql"]) else {
            panic!("expected a psql command");
        };
        assert!(args.is_empty());
    }

    #[test]
    fn down_cannot_both_keep_and_wipe() {
        assert!(Cli::try_parse_from(["popgres", "down", "--keep", "--wipe"]).is_err());
    }
}
