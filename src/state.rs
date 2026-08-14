//! Per-project instance state: where it lives and what's in it.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const LOCK_FILE: &str = ".lock";

/// An exclusive advisory lock for one project's state transitions.
///
/// The lock file remains in the state directory when the database is wiped so
/// every process continues to lock the same inode. Dropping this value releases
/// the lock.
pub struct StateLock {
    _file: File,
}

impl StateLock {
    pub fn acquire(state_dir: &Path, json: bool) -> Result<Self> {
        std::fs::create_dir_all(state_dir)
            .with_context(|| format!("cannot create {}", state_dir.display()))?;
        let path = state_dir.join(LOCK_FILE);
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(&path)
            .with_context(|| format!("cannot open state lock {}", path.display()))?;
        // A blocked lock can mean minutes (another popgres downloading binaries
        // or running a seed) — say so instead of hanging silently.
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                crate::commands::emit_event(
                    json,
                    serde_json::json!({ "event": "waiting_for_lock" }),
                    "popgres: waiting for another popgres process to finish...",
                );
                file.lock().with_context(|| {
                    format!("cannot lock state directory {}", state_dir.display())
                })?;
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(error).with_context(|| {
                    format!("cannot lock state directory {}", state_dir.display())
                });
            }
        }
        Ok(Self { _file: file })
    }
}

/// Everything we need to find, reconnect to, and tear down an instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Postmaster PID captured at startup. Missing in state written by v0.1.0.
    #[serde(default)]
    pub postmaster_pid: Option<u32>,
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
        write_private(&path, &json).with_context(|| format!("cannot write {}", path.display()))
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

/// Remove all instance data while preserving the stable advisory lock file.
pub fn wipe_state_dir(state_dir: &Path) -> Result<()> {
    if !state_dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(state_dir)
        .with_context(|| format!("cannot read {}", state_dir.display()))?
    {
        let entry = entry.with_context(|| format!("cannot read {}", state_dir.display()))?;
        if entry.file_name() == LOCK_FILE {
            continue;
        }
        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("cannot inspect {}", path.display()))?;
        if file_type.is_dir() {
            std::fs::remove_dir_all(&path)
                .with_context(|| format!("cannot remove {}", path.display()))?;
        } else {
            std::fs::remove_file(&path)
                .with_context(|| format!("cannot remove {}", path.display()))?;
        }
    }
    Ok(())
}

/// Write a file that may contain credentials so only its owner can read it.
pub(crate) fn write_private(path: &Path, contents: &str) -> Result<()> {
    let mut file = private_open_options().open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(format!("{contents}\n").as_bytes())?;
    Ok(())
}

fn private_open_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
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
            postmaster_pid: Some(12345),
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

    #[cfg(unix)]
    #[test]
    fn state_files_are_readable_only_by_their_owner() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        sample().save(dir.path()).unwrap();

        let mode = std::fs::metadata(path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn state_lock_serializes_callers() {
        use std::sync::mpsc;
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let first = StateLock::acquire(dir.path(), false).unwrap();
        let path = dir.path().to_path_buf();
        let (tx, rx) = mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let _second = StateLock::acquire(&path, false).unwrap();
            tx.send(()).unwrap();
        });

        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
        drop(first);
        rx.recv_timeout(Duration::from_secs(2)).unwrap();
        waiter.join().unwrap();
    }

    #[test]
    fn wiping_state_preserves_only_the_lock_file() {
        let dir = tempfile::tempdir().unwrap();
        let lock = StateLock::acquire(dir.path(), false).unwrap();
        std::fs::create_dir_all(dir.path().join("data/nested")).unwrap();
        std::fs::write(dir.path().join("data/nested/file"), "data").unwrap();
        std::fs::write(dir.path().join("state.json"), "state").unwrap();

        wipe_state_dir(dir.path()).unwrap();

        assert!(dir.path().join(LOCK_FILE).exists());
        assert!(!dir.path().join("data").exists());
        assert!(!dir.path().join("state.json").exists());
        drop(lock);
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
    fn state_from_v0_1_without_a_postmaster_pid_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let mut json = serde_json::to_value(sample()).unwrap();
        json.as_object_mut().unwrap().remove("postmaster_pid");
        std::fs::write(
            dir.path().join("state.json"),
            serde_json::to_string(&json).unwrap(),
        )
        .unwrap();

        let loaded = InstanceState::load(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.postmaster_pid, None);
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
