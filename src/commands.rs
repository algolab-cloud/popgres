//! One function per subcommand: everything the CLI actually does.

use std::time::Duration;

use anyhow::{Context, Result};

use crate::instance::{self, instance_env, instance_is_running, psql_binary, Started};
use crate::project::Project;
use crate::state::InstanceState;

/// How long a child gets to wind down on its own after Ctrl-C before we insist.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

pub async fn up(keep: bool, port: Option<u16>, pg: Option<String>, json: bool) -> Result<bool> {
    let project = Project::discover()?;
    let started = instance::start(&project, keep, port, pg, json).await?;
    emit_up(&started.state, json, started.already_running);
    Ok(started.already_running)
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

pub async fn run(
    keep: bool,
    port: Option<u16>,
    pg: Option<String>,
    json: bool,
    cmd: Vec<String>,
) -> Result<()> {
    let project = Project::discover()?;
    let Started {
        state,
        already_running,
    } = instance::start(&project, keep, port, pg, json).await?;

    if json {
        eprintln!(
            "{}",
            serde_json::json!({
                "event": "ready",
                "url": state.url(),
                "host": state.host,
                "port": state.port,
                "database": state.database,
                "already_running": already_running,
            })
        );
    } else if already_running {
        eprintln!(
            "popgres: reusing the instance on port {} — it will be left running",
            state.port
        );
    } else {
        eprintln!("popgres: up on port {} — DATABASE_URL is set", state.port);
    }

    // The child owns stdout: everything popgres says here goes to stderr.
    let child = tokio::process::Command::new(&cmd[0])
        .args(&cmd[1..])
        .envs(instance_env(&state))
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(error) => {
            let spawn_error =
                anyhow::Error::new(error).context(format!("failed to run `{}`", cmd.join(" ")));
            return match cleanup_after_run(&project, &state, already_running, json).await {
                Ok(()) => Err(spawn_error),
                Err(cleanup_error) => Err(spawn_error.context(format!(
                    "also failed to clean up the database: {cleanup_error:#}"
                ))),
            };
        }
    };

    let status: Result<_> = tokio::select! {
        result = child.wait() => result.context("failed to wait for the child process"),
        () = shutdown_signal() => {
            // Ctrl-C reaches the whole foreground process group, so the child is
            // usually already on its way out — let it finish before insisting.
            match tokio::time::timeout(SHUTDOWN_GRACE, child.wait()).await {
                Ok(result) => result.context("failed to wait for the child process"),
                Err(_) => {
                    if json {
                        eprintln!("{}", serde_json::json!({ "event": "killing_child" }));
                    } else {
                        eprintln!("popgres: command did not exit, killing it");
                    }
                    child.start_kill().ok();
                    child.wait().await.context("failed to wait for the child process")
                }
            }
        }
    };

    let cleanup = cleanup_after_run(&project, &state, already_running, json).await;
    match (status, cleanup) {
        (Ok(status), Ok(())) => {
            let exit_code = child_exit_code(&status);
            if json {
                eprintln!(
                    "{}",
                    serde_json::json!({ "event": "exit", "exit_code": exit_code })
                );
            }
            std::process::exit(exit_code);
        }
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(cleanup_error)) => Err(error.context(format!(
            "also failed to clean up the database: {cleanup_error:#}"
        ))),
    }
}

/// Teardown runs however the child exited or whether it started at all.
/// `state.keep` is the resolved answer because keep can also come from config.
async fn cleanup_after_run(
    project: &Project,
    state: &InstanceState,
    already_running: bool,
    json: bool,
) -> Result<()> {
    if already_running {
        if json {
            eprintln!(
                "{}",
                serde_json::json!({ "event": "left_running", "already_running": true })
            );
        } else {
            eprintln!("popgres: leaving the instance that was already running");
        }
    } else {
        instance::stop(project, state, state.keep).await?;
        if json {
            eprintln!(
                "{}",
                serde_json::json!({
                    "event": "stopped",
                    "kept": state.keep,
                    "wiped": !state.keep,
                })
            );
        } else if state.keep {
            eprintln!("popgres: stopped (data kept)");
        } else {
            eprintln!("popgres: stopped and wiped — poof!");
        }
    }
    Ok(())
}

fn child_exit_code(status: &std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    1
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

pub async fn down(keep_flag: bool, wipe_flag: bool, json: bool) -> Result<()> {
    let project = Project::discover()?;
    let Some(state) = project.state()? else {
        return Err(crate::error::coded(
            crate::error::NOT_RUNNING,
            "no popgres instance found for this project (nothing to stop)",
        ));
    };

    // Without --wipe there is no way out of a `keep = true` config.
    let keep = (keep_flag || state.keep) && !wipe_flag;
    instance::stop(&project, &state, keep).await?;

    if json {
        println!("{}", serde_json::json!({ "stopped": true, "wiped": !keep }));
    } else if keep {
        eprintln!("popgres: stopped (data kept — next `up` resumes)");
    } else {
        eprintln!("popgres: stopped and wiped — poof!");
    }
    Ok(())
}

pub fn url(json: bool) -> Result<()> {
    let url = Project::discover()?.running_instance()?.url();
    if json {
        println!("{}", serde_json::json!({ "url": url }));
    } else {
        println!("{url}");
    }
    Ok(())
}

pub fn psql(args: Vec<String>) -> Result<()> {
    let state = Project::discover()?.running_instance()?;
    let binary = psql_binary(&state)?;
    let status = std::process::Command::new(&binary)
        .arg(state.url())
        .args(&args)
        .status()
        .with_context(|| format!("failed to run {}", binary.display()))?;
    std::process::exit(status.code().unwrap_or(1));
}

pub async fn reset(json: bool) -> Result<()> {
    let project = Project::discover()?;
    let Some(state) = project.state()? else {
        return Err(crate::error::coded(
            crate::error::NOT_RUNNING,
            "no popgres instance found for this project — run `popgres up` first",
        ));
    };

    // Reset means fresh, so the data goes even for a --keep instance. Holding on
    // to the port keeps the URL stable for anything already pointed at it.
    instance::stop(&project, &state, false).await?;
    let started = instance::start(
        &project,
        state.keep,
        Some(state.port),
        Some(state.pg_version.clone()),
        json,
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

pub fn status(json: bool) -> Result<bool> {
    let project = Project::discover()?;
    let state = project.state()?;
    let running = match state.as_ref() {
        Some(state) => instance_is_running(state)?,
        None => false,
    };

    if json {
        println!(
            "{}",
            serde_json::json!({
                "running": running,
                "url": state.as_ref().filter(|_| running).map(InstanceState::url),
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
    Ok(running)
}

#[cfg(test)]
mod tests {
    use super::child_exit_code;

    #[cfg(unix)]
    #[test]
    fn a_signaled_child_uses_the_shell_signal_exit_convention() {
        let status = std::process::Command::new("sh")
            .args(["-c", "kill -TERM $$"])
            .status()
            .unwrap();
        assert_eq!(child_exit_code(&status), 143);
    }
}
