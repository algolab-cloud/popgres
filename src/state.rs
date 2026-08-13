//! Per-project instance state: where it lives and what's in it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Everything we need to find, reconnect to, and tear down an instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceState {
    pub project_dir: String,
    pub data_dir: String,
    pub installation_dir: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub database: String,
    pub pg_version: String,
    /// Persist data across stops (set by `up --keep` or config).
    pub keep: bool,
}

impl InstanceState {
    /// The connection string. An instance without a password (the default) gets
    /// a URL without one, rather than an empty `:@`.
    pub fn url(&self) -> String {
        let credentials = if self.password.is_empty() {
            self.username.clone()
        } else {
            format!("{}:{}", self.username, self.password)
        };
        format!(
            "postgresql://{}@{}:{}/{}",
            credentials, self.host, self.port, self.database
        )
    }

    pub fn save(&self, state_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(state_dir)
            .with_context(|| format!("cannot create {}", state_dir.display()))?;
        let path = state_dir.join("state.json");
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, json).with_context(|| format!("cannot write {}", path.display()))?;
        Ok(())
    }

    pub fn load(state_dir: &Path) -> Result<Option<Self>> {
        let path = state_dir.join("state.json");
        if !path.exists() {
            return Ok(None);
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        let state = serde_json::from_str(&raw)
            .with_context(|| format!("corrupt state file {}", path.display()))?;
        Ok(Some(state))
    }
}

/// Stable per-project state directory: ~/.local/share/popgres/<hash>/ (platform equivalent).
pub fn project_state_dir(project_dir: &Path) -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("dev", "popgres", "popgres")
        .context("cannot determine a data directory on this platform")?;
    let mut hasher = Sha256::new();
    hasher.update(project_dir.to_string_lossy().as_bytes());
    let hash = hex(&hasher.finalize()[..6]);
    Ok(dirs.data_dir().join(hash))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
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
            password: "s3cr3t".to_string(),
            database: "db".to_string(),
            pg_version: "18.4.0".to_string(),
            keep: false,
        }
    }

    #[test]
    fn url_carries_a_password_when_there_is_one() {
        assert_eq!(
            sample().url(),
            "postgresql://postgres:s3cr3t@127.0.0.1:54329/db"
        );
    }

    #[test]
    fn url_omits_the_password_when_there_is_none() {
        let state = InstanceState {
            password: String::new(),
            ..sample()
        };
        assert_eq!(state.url(), "postgresql://postgres@127.0.0.1:54329/db");
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let state = sample();
        state.save(dir.path()).unwrap();

        let loaded = InstanceState::load(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.url(), state.url());
        assert_eq!(loaded.pg_version, state.pg_version);
        assert_eq!(loaded.keep, state.keep);
    }

    #[test]
    fn save_creates_the_state_dir() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("does/not/exist/yet");
        sample().save(&nested).unwrap();
        assert!(nested.join("state.json").exists());
    }

    #[test]
    fn load_is_none_when_there_is_no_instance() {
        let dir = tempfile::tempdir().unwrap();
        assert!(InstanceState::load(dir.path()).unwrap().is_none());
    }

    #[test]
    fn load_reports_a_corrupt_state_file_rather_than_panicking() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("state.json"), "{ not json").unwrap();
        assert!(InstanceState::load(dir.path()).is_err());
    }

    #[test]
    fn each_project_gets_its_own_state_dir() {
        let one = project_state_dir(Path::new("/tmp/project-one")).unwrap();
        let two = project_state_dir(Path::new("/tmp/project-two")).unwrap();
        assert_ne!(one, two);
        // Same project, same directory — this is what makes `up` then `down` work.
        assert_eq!(
            one,
            project_state_dir(Path::new("/tmp/project-one")).unwrap()
        );
    }
}
