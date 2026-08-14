//! One function per subcommand: everything the CLI actually does.

use std::time::Duration;

use anyhow::{Context, Result};

use crate::instance::{self, instance_env, instance_is_running, psql_binary, Started};
use crate::project::Project;
use crate::seed;
use crate::state::InstanceState;

/// How long a child gets to wind down on its own after Ctrl-C before we insist.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// One lifecycle message, in whichever shape the invocation asked for.
/// JSON events and human messages both go to stderr; stdout stays reserved
/// for results (and for `run`'s child).
pub(crate) fn emit_event(json: bool, event: serde_json::Value, human: &str) {
    if json {
        eprintln!("{event}");
    } else {
        eprintln!("{human}");
    }
}

/// The one JSON shape describing a started instance, shared by `up` and
/// `run`'s "ready" event so the two can never drift apart.
fn instance_payload(state: &InstanceState, already_running: bool) -> serde_json::Value {
    serde_json::json!({
        "url": state.url(),
        "host": state.host,
        "port": state.port,
        "database": state.database,
        "expires_at": state.expires_at,
        "already_running": already_running,
    })
}

/// Run the seed hook against a freshly initialized database.
fn seed_if_fresh(project: &Project, started: &Started, json: bool) -> Result<()> {
    if started.freshly_initialized {
        if let Some(recipe) = project.config.seed.as_deref() {
            seed::run(project, &started.state, recipe, json)?;
        }
    }
    Ok(())
}

fn with_cleanup_failure(primary: anyhow::Error, cleanup_error: anyhow::Error) -> anyhow::Error {
    primary.context(format!(
        "also failed to clean up the database: {cleanup_error:#}"
    ))
}

pub async fn up(
    keep: bool,
    port: Option<u16>,
    pg: Option<String>,
    ttl: Option<String>,
    json: bool,
) -> Result<bool> {
    let project = Project::discover()?;
    let started = instance::start(&project, keep, port, pg, ttl, json).await?;
    // A failed seed leaves the instance up on purpose: the error says so, and
    // the user can inspect or fix and re-seed. `run` is the disposal command.
    seed_if_fresh(&project, &started, json)?;
    emit_up(&started.state, json, started.already_running);
    Ok(started.already_running)
}

fn emit_up(state: &InstanceState, json: bool, already: bool) {
    if json {
        println!("{}", instance_payload(state, already));
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
    ttl: Option<String>,
    json: bool,
    cmd: Vec<String>,
) -> Result<()> {
    let project = Project::discover()?;
    let started = instance::start(&project, keep, port, pg, ttl, json).await?;
    let already_running = started.already_running;

    // `run` promises disposal, so anything that fails between here and the
    // child's exit — seeding included — must still tear the instance down.
    if let Err(seed_error) = seed_if_fresh(&project, &started, json) {
        return match cleanup_after_run(&project, already_running, json).await {
            Ok(()) => Err(seed_error),
            Err(cleanup_error) => Err(with_cleanup_failure(seed_error, cleanup_error)),
        };
    }
    let state = started.state;

    if json {
        let mut ready = instance_payload(&state, already_running);
        ready["event"] = "ready".into();
        eprintln!("{ready}");
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
            return match cleanup_after_run(&project, already_running, json).await {
                Ok(()) => Err(spawn_error),
                Err(cleanup_error) => Err(with_cleanup_failure(spawn_error, cleanup_error)),
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
                    emit_event(
                        json,
                        serde_json::json!({ "event": "killing_child" }),
                        "popgres: command did not exit, killing it",
                    );
                    child.start_kill().ok();
                    child.wait().await.context("failed to wait for the child process")
                }
            }
        }
    };

    let cleanup = cleanup_after_run(&project, already_running, json).await;
    match (status, cleanup) {
        (Ok(status), cleanup) => {
            // The child's result is the point of the whole invocation — report
            // it even when the teardown afterwards failed.
            let exit_code = child_exit_code(&status);
            if json {
                eprintln!(
                    "{}",
                    serde_json::json!({ "event": "exit", "exit_code": exit_code })
                );
            }
            match cleanup {
                Ok(()) => std::process::exit(exit_code),
                Err(cleanup_error) => Err(cleanup_error.context(format!(
                    "the command exited with code {exit_code}, but cleaning up the database failed"
                ))),
            }
        }
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup_error)) => Err(with_cleanup_failure(error, cleanup_error)),
    }
}

/// Teardown runs however the child exited or whether it started at all.
///
/// The state is reloaded rather than reusing what `start` returned: the child
/// may have legitimately replaced the instance (`popgres reset` between test
/// suites), and `run`'s promise of disposal covers whatever the project's
/// instance is by the time it exits.
async fn cleanup_after_run(project: &Project, already_running: bool, json: bool) -> Result<()> {
    if already_running {
        emit_event(
            json,
            serde_json::json!({ "event": "left_running", "already_running": true }),
            "popgres: leaving the instance that was already running",
        );
        return Ok(());
    }
    let Some(current) = project.state()? else {
        return project.clear_env_file();
    };
    // `keep` can have been re-resolved by whatever the child did; honor it.
    instance::stop(project, &current, current.keep, json).await?;
    emit_event(
        json,
        serde_json::json!({
            "event": "stopped",
            "kept": current.keep,
            "wiped": !current.keep,
        }),
        if current.keep {
            "popgres: stopped (data kept)"
        } else {
            "popgres: stopped and wiped — poof!"
        },
    );
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
    instance::stop(&project, &state, keep, json).await?;

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
    std::process::exit(child_exit_code(&status));
}

pub async fn reset(json: bool) -> Result<()> {
    let project = Project::discover()?;
    let Some(state) = project.state()? else {
        return Err(crate::error::coded(
            crate::error::NOT_RUNNING,
            "no popgres instance found for this project — run `popgres up` first",
        ));
    };

    // Reset means fresh, so the data goes even for a --keep instance. The
    // stop and start happen under one lock, keeping the port — and so the
    // URL — stable for anything already pointed at it.
    let started = instance::reset(&project, &state, json).await?;
    seed_if_fresh(&project, &started, json)?;

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

/// Dispose of every instance past its TTL, in this project and any other.
///
/// This is the only command that touches instances outside the current
/// project, and it never destroys anything that has not expired.
/// Survey every instance on this machine — the read-only counterpart to `gc`.
///
/// Connection URLs are deliberately omitted: this walks every project, and a
/// configured password would otherwise be printed for all of them at once.
/// Use `popgres url` for the current project.
pub fn list(json: bool) -> Result<()> {
    let entries = instance::list()?;
    // Knowing which row is "here" is the common question; a failure to work
    // that out (deleted cwd) must not sink the survey.
    let here = Project::discover().ok().map(|project| project.state_dir);

    if json {
        let rows: Vec<_> = entries
            .iter()
            .map(|entry| match entry {
                instance::Entry::Instance {
                    state_dir,
                    state,
                    liveness,
                } => serde_json::json!({
                    "state_dir": state_dir.display().to_string(),
                    "project_dir": state.project_dir,
                    "current": here.as_deref() == Some(state_dir.as_path()),
                    "status": liveness.label(),
                    "running": matches!(liveness, instance::Liveness::Running),
                    "port": state.port,
                    "pg_version": state.pg_version,
                    "database": state.database,
                    "keep": state.keep,
                    "expires_at": state.expires_at,
                    "expired": state.is_expired(),
                }),
                instance::Entry::Unreadable { state_dir, reason } => serde_json::json!({
                    "state_dir": state_dir.display().to_string(),
                    "status": "unreadable",
                    "reason": reason,
                }),
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({ "instances": rows, "count": rows.len() })
        );
        return Ok(());
    }

    if entries.is_empty() {
        println!("no instances");
        return Ok(());
    }

    let mut rows = Vec::new();
    for entry in &entries {
        match entry {
            instance::Entry::Instance {
                state_dir,
                state,
                liveness,
            } => rows.push([
                if here.as_deref() == Some(state_dir.as_path()) {
                    "*".to_string()
                } else {
                    String::new()
                },
                liveness.label().to_string(),
                state.port.to_string(),
                state.pg_version.trim_start_matches('=').to_string(),
                ttl_column(state),
                state.project_dir.clone(),
            ]),
            instance::Entry::Unreadable { state_dir, reason } => rows.push([
                String::new(),
                "unreadable".to_string(),
                "—".to_string(),
                "—".to_string(),
                "—".to_string(),
                format!("{} ({reason})", state_dir.display()),
            ]),
        }
    }

    let headers = ["", "STATUS", "PORT", "VERSION", "TTL", "PROJECT"];
    // The variable-width project path goes last so nothing else can misalign.
    let widths: Vec<usize> = (0..headers.len())
        .map(|column| {
            rows.iter()
                .map(|row| row[column].chars().count())
                .chain(std::iter::once(headers[column].len()))
                .max()
                .unwrap_or(0)
        })
        .collect();
    let render = |cells: &[String]| {
        let line: Vec<String> = cells
            .iter()
            .enumerate()
            .map(|(column, cell)| {
                if column + 1 == cells.len() {
                    cell.clone()
                } else {
                    format!("{cell:<width$}", width = widths[column])
                }
            })
            .collect();
        println!("{}", line.join("  ").trim_end());
    };
    render(&headers.map(str::to_string));
    for row in &rows {
        render(row);
    }
    Ok(())
}

/// How much life an instance has left, for the human table.
fn ttl_column(state: &crate::state::InstanceState) -> String {
    ttl_label(state.expires_at, crate::state::now_unix())
}

/// Pure so the formatting can be tested without racing the clock.
fn ttl_label(expires_at: Option<u64>, now: u64) -> String {
    let Some(deadline) = expires_at else {
        return "—".to_string();
    };
    if now >= deadline {
        return "expired".to_string();
    }
    let remaining = deadline - now;
    if remaining >= 86_400 {
        format!("{}d", remaining / 86_400)
    } else if remaining >= 3_600 {
        format!("{}h", remaining / 3_600)
    } else if remaining >= 60 {
        format!("{}m", remaining / 60)
    } else {
        format!("{remaining}s")
    }
}

pub async fn gc(dry_run: bool, json: bool) -> Result<()> {
    let swept = instance::gc(dry_run).await?;
    let mut reaped = Vec::new();
    for (state_dir, outcome) in &swept {
        match outcome {
            instance::Reaped::Expired { state } => {
                reaped.push(state.clone());
                emit_event(
                    json,
                    serde_json::json!({
                        "event": if dry_run { "would_reap" } else { "reaped" },
                        "project_dir": state.project_dir,
                        "port": state.port,
                        "kept": state.keep,
                    }),
                    &format!(
                        "popgres: {} the expired instance for {} (port {})",
                        if dry_run { "would reap" } else { "reaped" },
                        state.project_dir,
                        state.port
                    ),
                );
            }
            instance::Reaped::Skipped { reason } => emit_event(
                json,
                serde_json::json!({
                    "event": "skipped",
                    "state_dir": state_dir.display().to_string(),
                    "reason": reason,
                }),
                &format!("popgres: skipped {} — {reason}", state_dir.display()),
            ),
            instance::Reaped::Busy | instance::Reaped::Kept => {}
        }
    }

    if json {
        println!(
            "{}",
            serde_json::json!({
                "reaped": reaped.iter().map(|state| serde_json::json!({
                    "project_dir": state.project_dir,
                    "port": state.port,
                    "kept": state.keep,
                })).collect::<Vec<_>>(),
                "examined": swept.len(),
                "dry_run": dry_run,
            })
        );
    } else if reaped.is_empty() {
        println!("nothing to reap ({} instance(s) examined)", swept.len());
    } else if dry_run {
        println!(
            "would reap {} expired instance(s) — rerun without --dry-run",
            reaped.len()
        );
    } else {
        println!("reaped {} expired instance(s)", reaped.len());
    }
    Ok(())
}

pub fn status(json: bool) -> Result<bool> {
    let project = Project::discover()?;
    let state = project.state()?;
    let running = state.as_ref().is_some_and(instance_is_running);

    if json {
        println!(
            "{}",
            serde_json::json!({
                "running": running,
                "url": state.as_ref().filter(|_| running).map(InstanceState::url),
                "port": state.as_ref().map(|s| s.port),
                "pg_version": state.as_ref().map(|s| s.pg_version.clone()),
                "keep": state.as_ref().map(|s| s.keep),
                "expires_at": state.as_ref().and_then(|s| s.expires_at),
                "expired": state.as_ref().is_some_and(InstanceState::is_expired),
            })
        );
    } else if let Some(s) = state {
        if running {
            println!(
                "running — Postgres {} on port {}{}",
                s.pg_version.trim_start_matches('='),
                s.port,
                if s.is_expired() {
                    " (past its ttl — `popgres gc` will reap it)"
                } else {
                    ""
                }
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
    #[cfg(unix)]
    use super::child_exit_code;
    use super::ttl_label;

    // A fixed "now" keeps these deterministic: reading the clock twice —
    // once here and once inside the formatter — makes every boundary flaky.
    const NOW: u64 = 1_800_000_000;

    #[test]
    fn an_instance_without_a_deadline_shows_a_dash() {
        assert_eq!(ttl_label(None, NOW), "—");
    }

    #[test]
    fn a_deadline_at_or_before_now_reads_as_expired() {
        assert_eq!(ttl_label(Some(NOW - 1), NOW), "expired");
        assert_eq!(ttl_label(Some(NOW), NOW), "expired");
    }

    #[test]
    fn remaining_time_uses_the_largest_whole_unit() {
        assert_eq!(ttl_label(Some(NOW + 45), NOW), "45s");
        assert_eq!(ttl_label(Some(NOW + 90), NOW), "1m");
        assert_eq!(ttl_label(Some(NOW + 7_200), NOW), "2h");
        assert_eq!(ttl_label(Some(NOW + 172_800), NOW), "2d");
        // Boundaries round down to the unit that just became whole.
        assert_eq!(ttl_label(Some(NOW + 59), NOW), "59s");
        assert_eq!(ttl_label(Some(NOW + 60), NOW), "1m");
        assert_eq!(ttl_label(Some(NOW + 3_599), NOW), "59m");
        assert_eq!(ttl_label(Some(NOW + 86_400), NOW), "1d");
    }

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
