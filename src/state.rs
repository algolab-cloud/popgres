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
        let (file, _path) = Self::open(state_dir, true)?;
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

    /// The lock if it is free right now, `None` if someone else holds it.
    ///
    /// `gc` walks projects it does not own, so it must never block on one that
    /// is mid-start: a busy project is simply skipped and reaped next sweep.
    pub fn try_acquire(state_dir: &Path) -> Result<Option<Self>> {
        let (file, path) = Self::open(state_dir, true)?;
        Self::try_lock(file, path)
    }

    /// Try to lock state without creating any file or directory.
    ///
    /// A dry-run must be strictly read-only. Old state without the stable lock
    /// is therefore reported as unsafe to inspect instead of being modified.
    pub fn try_acquire_existing(state_dir: &Path) -> Result<Option<Self>> {
        let (file, path) = Self::open(state_dir, false)?;
        Self::try_lock(file, path)
    }

    /// How long a caller that must not block still waits out a lock that is
    /// only just being released. A lock the kernel has not finished dropping
    /// can report as held for a few milliseconds after its holder is gone, and
    /// treating that as "busy" makes `gc` skip a project for no reason.
    const RELEASE_GRACE: std::time::Duration = std::time::Duration::from_millis(50);
    const RELEASE_POLL: std::time::Duration = std::time::Duration::from_millis(5);

    fn try_lock(file: File, path: PathBuf) -> Result<Option<Self>> {
        let deadline = std::time::Instant::now() + Self::RELEASE_GRACE;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Some(Self { _file: file })),
                // Genuinely held work (a start, a seed) lasts far longer than
                // the grace period, so this still returns promptly for it.
                Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Self::RELEASE_POLL);
                }
                Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(error).with_context(|| format!("cannot lock {}", path.display()));
                }
            }
        }
    }

    fn open(state_dir: &Path, create: bool) -> Result<(File, PathBuf)> {
        if create {
            std::fs::create_dir_all(state_dir)
                .with_context(|| format!("cannot create {}", state_dir.display()))?;
        }
        let path = state_dir.join(LOCK_FILE);
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(create);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(&path)
            .with_context(|| format!("cannot open state lock {}", path.display()))?;
        Ok((file, path))
    }
}

/// Make a local `.popgres/` a good citizen of the project tree.
///
/// A `.gitignore` containing `*` keeps the whole directory out of version
/// control without touching the repository's own ignore file, and a
/// `CACHEDIR.TAG` tells backup and sync tools the contents are disposable.
/// Both survive wipes and are only written when missing, so a user who
/// deliberately edits them is left alone.
pub fn ensure_local_markers(state_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(state_dir)
        .with_context(|| format!("cannot create {}", state_dir.display()))?;
    let gitignore = state_dir.join(".gitignore");
    if !gitignore.exists() {
        std::fs::write(&gitignore, "*\n")
            .with_context(|| format!("cannot write {}", gitignore.display()))?;
    }
    let cachedir_tag = state_dir.join("CACHEDIR.TAG");
    if !cachedir_tag.exists() {
        std::fs::write(
            &cachedir_tag,
            "Signature: 8a477f597d28d172789f06886806bc55\n\
             # This directory holds a disposable popgres PostgreSQL instance.\n\
             # See https://bford.info/cachedir/\n",
        )
        .with_context(|| format!("cannot write {}", cachedir_tag.display()))?;
    }
    Ok(())
}

/// Seconds since the Unix epoch — how deadlines are recorded, so they survive
/// process exit, reboots, and clock-independent monotonic sources.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
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
    /// Unix-epoch second after which `popgres gc` may dispose of this
    /// instance. `None` means it lives until someone stops it.
    #[serde(default)]
    pub expires_at: Option<u64>,
    /// Extension names configured when this instance was created, sorted.
    /// A resume compares these against the config to catch drift.
    #[serde(default)]
    pub extensions: Vec<String>,
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

    /// The connection string for a specific database on this instance.
    pub fn url_for(&self, database: &str) -> String {
        let mut other = self.clone();
        other.database = database.to_string();
        other.url()
    }

    /// Whether this instance's TTL has run out.
    pub fn is_expired(&self) -> bool {
        self.expires_at
            .is_some_and(|deadline| now_unix() >= deadline)
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

/// Files that survive a wipe: the stable lock inode, plus the two markers
/// that make a local `.popgres/` behave well in a repository (self-ignoring)
/// and under backup tools (cache tag).
const WIPE_KEEPS: [&str; 3] = [LOCK_FILE, ".gitignore", "CACHEDIR.TAG"];

/// Remove all instance data while preserving the stable advisory lock file
/// and the local-directory markers.
pub fn wipe_state_dir(state_dir: &Path) -> Result<()> {
    if !state_dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(state_dir)
        .with_context(|| format!("cannot read {}", state_dir.display()))?
    {
        let entry = entry.with_context(|| format!("cannot read {}", state_dir.display()))?;
        if WIPE_KEEPS.iter().any(|keep| entry.file_name() == *keep) {
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
    let mut hasher = Sha256::new();
    hasher.update(project_dir.to_string_lossy().as_bytes());
    let hash = hex(&hasher.finalize()[..6]);
    Ok(popgres_data_dir()?.join(hash))
}

/// Where every project's state directory lives.
pub fn popgres_data_dir() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("dev", "popgres", "popgres")
        .context("cannot determine a data directory on this platform")?;
    Ok(dirs.data_dir().to_path_buf())
}

/// Every state directory popgres knows about, across all projects.
pub fn all_state_dirs() -> Result<Vec<PathBuf>> {
    let root = popgres_data_dir()?;
    state_dirs_in(&root)
}

fn state_dirs_in(root: &Path) -> Result<Vec<PathBuf>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut dirs = Vec::new();
    for entry in
        std::fs::read_dir(root).with_context(|| format!("cannot read {}", root.display()))?
    {
        let entry = entry.with_context(|| format!("cannot read {}", root.display()))?;
        // A state dir is one holding a state.json; anything else here (the
        // shared binary cache, stray files) is none of gc's business.
        if entry.path().join("state.json").is_file() {
            dirs.push(entry.path());
        }
    }
    dirs.sort();
    Ok(dirs)
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
            expires_at: None,
            extensions: Vec::new(),
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
    fn local_markers_are_written_once_and_survive_a_wipe() {
        let dir = tempfile::tempdir().unwrap();
        ensure_local_markers(dir.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".gitignore")).unwrap(),
            "*\n"
        );
        assert!(std::fs::read_to_string(dir.path().join("CACHEDIR.TAG"))
            .unwrap()
            .starts_with("Signature: 8a477f597d28d172789f06886806bc55"));

        // A user's deliberate edit is left alone.
        std::fs::write(dir.path().join(".gitignore"), "*\n!keep-me\n").unwrap();
        ensure_local_markers(dir.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".gitignore")).unwrap(),
            "*\n!keep-me\n"
        );

        // Wiping removes the instance but keeps the markers and the lock.
        std::fs::create_dir_all(dir.path().join("data")).unwrap();
        std::fs::write(dir.path().join("state.json"), "{}").unwrap();
        drop(StateLock::acquire(dir.path(), false).unwrap());
        wipe_state_dir(dir.path()).unwrap();
        assert!(!dir.path().join("data").exists());
        assert!(!dir.path().join("state.json").exists());
        assert!(dir.path().join(".gitignore").exists());
        assert!(dir.path().join("CACHEDIR.TAG").exists());
        assert!(dir.path().join(".lock").exists());
    }

    #[test]
    fn an_instance_without_a_ttl_never_expires() {
        assert!(!sample().is_expired());
    }

    #[test]
    fn a_deadline_in_the_past_is_expired_and_one_ahead_is_not() {
        let past = InstanceState {
            expires_at: Some(now_unix() - 1),
            ..sample()
        };
        let future = InstanceState {
            expires_at: Some(now_unix() + 3600),
            ..sample()
        };
        assert!(past.is_expired());
        assert!(!future.is_expired());
    }

    #[test]
    fn a_deadline_survives_the_state_file_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let state = InstanceState {
            expires_at: Some(1_800_000_000),
            ..sample()
        };
        state.save(dir.path()).unwrap();
        let loaded = InstanceState::load(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.expires_at, Some(1_800_000_000));
    }

    #[test]
    fn state_written_before_ttl_existed_still_loads() {
        // v0.1.0 state has neither postmaster_pid nor expires_at.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("state.json"),
            r#"{"project_dir":"/p","data_dir":"/p/data","installation_dir":"/i",
                "host":"127.0.0.1","port":5432,"username":"postgres","password":"",
                "database":"db","pg_version":"18.4.0","keep":false}"#,
        )
        .unwrap();

        let loaded = InstanceState::load(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.expires_at, None);
        assert!(!loaded.is_expired());
    }

    #[test]
    fn a_just_released_lock_is_not_mistaken_for_a_busy_one() {
        // Releasing a flock is not always visible to the next attempt
        // immediately, and treating that instant as "busy" makes `gc` skip a
        // project that nothing is using. Hammer the handoff to prove it holds.
        let handles: Vec<_> = (0..8)
            .map(|_| {
                std::thread::spawn(|| {
                    let dir = tempfile::tempdir().unwrap();
                    for _ in 0..100 {
                        drop(StateLock::acquire(dir.path(), false).unwrap());
                        assert!(
                            StateLock::try_acquire(dir.path()).unwrap().is_some(),
                            "a released lock was reported busy"
                        );
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
    }

    #[test]
    fn a_held_lock_is_reported_busy_rather_than_blocking() {
        // What keeps `gc` from stalling on a project mid-start.
        let dir = tempfile::tempdir().unwrap();
        let held = StateLock::acquire(dir.path(), false).unwrap();
        let path = dir.path().to_path_buf();

        let busy = std::thread::spawn(move || StateLock::try_acquire(&path).unwrap().is_none())
            .join()
            .unwrap();

        assert!(busy);
        drop(held);
    }

    #[test]
    fn only_directories_holding_state_are_swept() {
        // all_state_dirs must ignore the shared binary cache and stray files.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("with-state")).unwrap();
        std::fs::write(dir.path().join("with-state/state.json"), "{}").unwrap();
        std::fs::create_dir_all(dir.path().join("no-state")).unwrap();

        let found = state_dirs_in(dir.path()).unwrap();
        assert_eq!(found, [dir.path().join("with-state")]);
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
