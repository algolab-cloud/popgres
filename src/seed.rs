//! The `seed` hook: what runs once, against a freshly created database.

use anyhow::{bail, Context, Result};

use crate::instance::{instance_env, psql_binary};
use crate::project::Project;
use crate::state::InstanceState;

/// Run the configured `seed`: a `.sql` file goes through psql, anything else
/// runs as a shell command with `DATABASE_URL` set.
pub fn run(project: &Project, state: &InstanceState, recipe: &str, quiet: bool) -> Result<()> {
    let path = project.root.join(recipe);
    let is_sql_file = path.is_file()
        && path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("sql"));

    if !quiet {
        eprintln!("popgres: seeding from {recipe}");
    }
    let status = if is_sql_file {
        std::process::Command::new(psql_binary(state)?)
            .arg(state.url())
            .args(["--quiet", "--no-psqlrc", "-v", "ON_ERROR_STOP=1", "-f"])
            .arg(&path)
            .current_dir(&project.root)
            .status()
            .with_context(|| format!("failed to run the seed file {recipe}"))?
    } else {
        let (shell, flag) = if cfg!(windows) {
            ("cmd", "/C")
        } else {
            ("sh", "-c")
        };
        std::process::Command::new(shell)
            .arg(flag)
            .arg(recipe)
            .envs(instance_env(state))
            .current_dir(&project.root)
            .status()
            .with_context(|| format!("failed to run the seed command `{recipe}`"))?
    };

    if !status.success() {
        bail!("seed `{recipe}` failed ({status}) — the database is up but not seeded");
    }
    Ok(())
}
