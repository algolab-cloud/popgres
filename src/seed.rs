//! The `seed` hook: what runs once, against a freshly created database.

use anyhow::{bail, Context, Result};

use crate::instance::{instance_env, psql_binary};
use crate::project::Project;
use crate::state::InstanceState;

/// Run the configured `seed`: a `.sql` file goes through psql, anything else
/// runs as a shell command with `DATABASE_URL` set.
pub fn run(project: &Project, state: &InstanceState, recipe: &str, json: bool) -> Result<()> {
    let path = project.root.join(recipe);
    let is_sql_file = path.is_file()
        && path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("sql"));

    crate::commands::emit_event(
        json,
        serde_json::json!({ "event": "seeding", "recipe": recipe }),
        &format!("popgres: seeding from {recipe}"),
    );
    let mut command = if is_sql_file {
        let mut command = std::process::Command::new(psql_binary(state)?);
        command
            .arg(state.url())
            .args(["--quiet", "--no-psqlrc", "-v", "ON_ERROR_STOP=1", "-f"])
            .arg(&path);
        command
    } else {
        let (shell, flag) = if cfg!(windows) {
            ("cmd", "/C")
        } else {
            ("sh", "-c")
        };
        let mut command = std::process::Command::new(shell);
        command.arg(flag).arg(recipe).envs(instance_env(state));
        command
    };
    command.current_dir(&project.root);

    // In JSON mode stdout is reserved for results, so the seed's chatter must
    // not land there — capture it and replay everything on stderr.
    let status = if json {
        let output = command
            .output()
            .with_context(|| format!("failed to run the seed `{recipe}`"))?;
        std::io::Write::write_all(&mut std::io::stderr(), &output.stdout).ok();
        std::io::Write::write_all(&mut std::io::stderr(), &output.stderr).ok();
        output.status
    } else {
        command
            .status()
            .with_context(|| format!("failed to run the seed `{recipe}`"))?
    };

    if !status.success() {
        bail!("seed `{recipe}` failed ({status}) — the database is up but not seeded");
    }
    Ok(())
}
