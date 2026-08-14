//! The project popgres is acting on: where it is, what it configured, and the
//! env file it wants `DATABASE_URL` written into.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config::{self, Config, Location};
use crate::instance::instance_is_running;
use crate::registry::LOCAL_STATE_DIR;
use crate::state::{project_state_dir, write_private, InstanceState};
use crate::ENV_VAR;

/// The project the current directory belongs to, its optional `popgres.toml`,
/// and where its instance lives.
pub struct Project {
    pub root: PathBuf,
    pub state_dir: PathBuf,
    pub config: Config,
    /// The instance lives in `.popgres/` inside the project rather than in
    /// the per-user data directory.
    pub local: bool,
}

impl Project {
    pub fn discover() -> Result<Self> {
        let cwd = std::env::current_dir()
            .context("cannot determine current directory")?
            .canonicalize()
            .context("cannot canonicalize current directory")?;
        let (root, config) = config::discover(&cwd)?;
        Self::assemble(root, config)
    }

    /// The project rooted at a known directory, for acting on an instance
    /// recorded by another working directory (as `gc` does).
    pub fn at(root: &Path) -> Result<Self> {
        let config = config::at(root)?;
        Self::assemble(root.to_path_buf(), config)
    }

    fn assemble(root: PathBuf, config: Config) -> Result<Self> {
        let (state_dir, local) = resolve_state_dir(&root, &config)?;
        Ok(Self {
            root,
            state_dir,
            config,
            local,
        })
    }

    pub fn state(&self) -> Result<Option<InstanceState>> {
        InstanceState::load(&self.state_dir)
    }

    /// The state of a running instance, or a clear error explaining there isn't one.
    pub fn running_instance(&self) -> Result<InstanceState> {
        let Some(state) = self.state()? else {
            return Err(crate::error::coded(
                crate::error::NOT_RUNNING,
                "no popgres instance found for this project — run `popgres up` first",
            ));
        };
        if !instance_is_running(&state) {
            return Err(crate::error::coded(
                crate::error::NOT_RUNNING,
                "instance is not running — run `popgres up`",
            ));
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

/// Where this project's instance lives.
///
/// Local (`.popgres/` in the project) is the default. One exception keeps
/// upgrades seamless: a project whose instance predates local-by-default
/// still has state in the per-user data directory — that instance is honored
/// until it is wiped, and the next fresh start creates locally. A local
/// instance, once it exists, always wins.
fn resolve_state_dir(root: &Path, config: &Config) -> Result<(PathBuf, bool)> {
    let global = project_state_dir(root)?;
    match config.resolved_location() {
        Location::Global => Ok((global, false)),
        Location::Local => {
            let local = root.join(LOCAL_STATE_DIR);
            if !local.join("state.json").is_file() && global.join("state.json").is_file() {
                Ok((global, false))
            } else {
                Ok((local, true))
            }
        }
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
    fn a_project_defaults_to_a_local_popgres_directory() {
        let dir = tempfile::tempdir().unwrap();
        let (state_dir, local) = resolve_state_dir(dir.path(), &Config::default()).unwrap();
        assert_eq!(state_dir, dir.path().join(LOCAL_STATE_DIR));
        assert!(local);
    }

    #[test]
    fn location_global_keeps_the_project_tree_clean() {
        let dir = tempfile::tempdir().unwrap();
        let config: Config = toml::from_str(r#"location = "global""#).unwrap();
        let (state_dir, local) = resolve_state_dir(dir.path(), &config).unwrap();
        assert!(!local);
        assert!(!state_dir.starts_with(dir.path()));
        assert_eq!(state_dir, project_state_dir(dir.path()).unwrap());
    }

    #[test]
    fn an_unknown_location_is_an_error_rather_than_silence() {
        assert!(toml::from_str::<Config>(r#"location = "cloud""#).is_err());
    }

    #[test]
    fn a_pre_existing_global_instance_is_honored_until_wiped() {
        // Upgrading must not strand a running or kept instance: with global
        // state present and no local one, the global dir stays in charge.
        let dir = tempfile::tempdir().unwrap();
        let global = project_state_dir(dir.path()).unwrap();
        std::fs::create_dir_all(&global).unwrap();
        std::fs::write(global.join("state.json"), "{}").unwrap();

        let (state_dir, local) = resolve_state_dir(dir.path(), &Config::default()).unwrap();
        assert_eq!(state_dir, global);
        assert!(!local);

        // Once the legacy state is wiped, the next start is local.
        std::fs::remove_file(global.join("state.json")).unwrap();
        let (state_dir, local) = resolve_state_dir(dir.path(), &Config::default()).unwrap();
        assert_eq!(state_dir, dir.path().join(LOCAL_STATE_DIR));
        assert!(local);
        std::fs::remove_dir_all(&global).ok();
    }

    #[test]
    fn a_local_instance_wins_over_a_leftover_global_one() {
        let dir = tempfile::tempdir().unwrap();
        let global = project_state_dir(dir.path()).unwrap();
        std::fs::create_dir_all(&global).unwrap();
        std::fs::write(global.join("state.json"), "{}").unwrap();
        let local_dir = dir.path().join(LOCAL_STATE_DIR);
        std::fs::create_dir_all(&local_dir).unwrap();
        std::fs::write(local_dir.join("state.json"), "{}").unwrap();

        let (state_dir, local) = resolve_state_dir(dir.path(), &Config::default()).unwrap();
        assert_eq!(state_dir, local_dir);
        assert!(local);
        std::fs::remove_dir_all(&global).ok();
    }

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
