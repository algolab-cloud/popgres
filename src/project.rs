//! The project popgres is acting on: where it is, what it configured, and the
//! env file it wants `DATABASE_URL` written into.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::config::{self, Config};
use crate::instance::instance_is_running;
use crate::state::{project_state_dir, write_private, InstanceState};
use crate::ENV_VAR;

/// The project the current directory belongs to, its optional `popgres.toml`,
/// and where its instance lives.
pub struct Project {
    pub root: PathBuf,
    pub state_dir: PathBuf,
    pub config: Config,
}

impl Project {
    pub fn discover() -> Result<Self> {
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

    pub fn state(&self) -> Result<Option<InstanceState>> {
        InstanceState::load(&self.state_dir)
    }

    /// The state of a running instance, or a clear error explaining there isn't one.
    pub fn running_instance(&self) -> Result<InstanceState> {
        let Some(state) = self.state()? else {
            bail!("no popgres instance found for this project — run `popgres up` first");
        };
        if !instance_is_running(&state)? {
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
    pub fn write_env_file(&self, url: &str) -> Result<()> {
        let Some(path) = self.env_file() else {
            return Ok(());
        };
        let mut lines = env_file_lines_without_url(&path)?;
        lines.push(format!("{ENV_VAR}={url}"));
        write_private(&path, &lines.join("\n"))
            .with_context(|| format!("cannot write {}", path.display()))
    }

    /// Drop the dead URL from the `env_file` once the instance is gone.
    pub fn clear_env_file(&self) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writing_the_url_leaves_other_variables_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env.local");
        std::fs::write(&path, "API_KEY=abc\nDATABASE_URL=stale\nDEBUG=1\n").unwrap();

        let kept = env_file_lines_without_url(&path).unwrap();
        assert_eq!(kept, ["API_KEY=abc", "DEBUG=1"]);
    }

    #[test]
    fn a_missing_env_file_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        let lines = env_file_lines_without_url(&dir.path().join("nope")).unwrap();
        assert!(lines.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn env_files_are_readable_only_by_their_owner() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        write_private(
            &path,
            "DATABASE_URL=postgresql://postgres@127.0.0.1:5432/db",
        )
        .unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(std::fs::read_to_string(&path).unwrap().ends_with('\n'));
    }
}
