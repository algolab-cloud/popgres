//! The global registry of projects with local instances.
//!
//! `list` and `gc` walk the global state root to find instances; a local
//! `.popgres/` is invisible to that walk, so `up` records the project path
//! here and the machine-wide commands read it back. The registry is
//! bookkeeping, not truth: it is pruned on read (an entry whose instance is
//! gone disappears), corrupt contents are treated as empty, and losing it
//! only means `list` misses local instances until their next `up`.

use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const REGISTRY_FILE: &str = "registry.json";

/// The directory a local instance lives in, under the project root.
pub const LOCAL_STATE_DIR: &str = ".popgres";

#[derive(Debug, Default, Serialize, Deserialize)]
struct Registry {
    projects: Vec<String>,
}

/// Record a project as having a local instance. Idempotent.
pub fn register(project_root: &Path) -> Result<()> {
    let path = registry_path()?;
    with_locked(&path, |registry| {
        let entry = project_root.display().to_string();
        if registry.projects.contains(&entry) {
            (false, ())
        } else {
            registry.projects.push(entry);
            (true, ())
        }
    })
}

/// State directories of every registered project that still holds an
/// instance. Entries whose instance is gone are pruned as they are seen —
/// deleted projects fall out of `list` instead of leaking forever.
pub fn local_state_dirs() -> Result<Vec<PathBuf>> {
    let path = registry_path()?;
    with_locked(&path, |registry| {
        let mut dirs = Vec::new();
        let before = registry.projects.len();
        registry.projects.retain(|project| {
            let state_dir = Path::new(project).join(LOCAL_STATE_DIR);
            if state_dir.join("state.json").is_file() {
                dirs.push(state_dir);
                true
            } else {
                false
            }
        });
        (registry.projects.len() != before, dirs)
    })
}

fn registry_path() -> Result<PathBuf> {
    Ok(crate::state::popgres_data_dir()?.join(REGISTRY_FILE))
}

/// Read-modify-write under an exclusive lock on the registry file itself.
///
/// The write is in place rather than write-and-rename: a rename swaps the
/// inode out from under whoever is waiting on the lock, which is a lost
/// update. In-place risks only a torn write on a crash mid-write, and a
/// corrupt registry already reads as empty.
fn with_locked<R>(path: &Path, apply: impl FnOnce(&mut Registry) -> (bool, R)) -> Result<R> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    file.lock()
        .with_context(|| format!("cannot lock {}", path.display()))?;

    let mut raw = String::new();
    file.read_to_string(&mut raw)
        .with_context(|| format!("cannot read {}", path.display()))?;
    let mut registry: Registry = serde_json::from_str(&raw).unwrap_or_default();

    let (changed, output) = apply(&mut registry);
    if changed {
        registry.projects.sort();
        registry.projects.dedup();
        let json = serde_json::to_string_pretty(&registry)?;
        file.rewind()?;
        file.set_len(0)?;
        file.write_all(json.as_bytes())
            .with_context(|| format!("cannot write {}", path.display()))?;
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(path: &Path) -> Registry {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn register_at(path: &Path, project_root: &Path) {
        with_locked(path, |registry| {
            let entry = project_root.display().to_string();
            if registry.projects.contains(&entry) {
                (false, ())
            } else {
                registry.projects.push(entry);
                (true, ())
            }
        })
        .unwrap();
    }

    #[test]
    fn registering_twice_records_one_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("registry.json");
        register_at(&path, Path::new("/tmp/project"));
        register_at(&path, Path::new("/tmp/project"));
        assert_eq!(read(&path).projects, ["/tmp/project"]);
    }

    #[test]
    fn a_corrupt_registry_reads_as_empty_and_heals() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("registry.json");
        std::fs::write(&path, "{ not json").unwrap();
        register_at(&path, Path::new("/tmp/project"));
        assert_eq!(read(&path).projects, ["/tmp/project"]);
    }

    #[test]
    fn pruning_drops_projects_whose_instance_is_gone() {
        let registry_dir = tempfile::tempdir().unwrap();
        let path = registry_dir.path().join("registry.json");

        let alive = tempfile::tempdir().unwrap();
        let state_dir = alive.path().join(LOCAL_STATE_DIR);
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(state_dir.join("state.json"), "{}").unwrap();

        register_at(&path, alive.path());
        register_at(&path, Path::new("/tmp/deleted-project"));

        let dirs = with_locked(&path, |registry| {
            let mut dirs = Vec::new();
            let before = registry.projects.len();
            registry.projects.retain(|project| {
                let state_dir = Path::new(project).join(LOCAL_STATE_DIR);
                if state_dir.join("state.json").is_file() {
                    dirs.push(state_dir);
                    true
                } else {
                    false
                }
            });
            (registry.projects.len() != before, dirs)
        })
        .unwrap();

        assert_eq!(dirs, [state_dir]);
        assert_eq!(read(&path).projects, [alive.path().display().to_string()]);
    }

    #[test]
    fn a_shorter_registry_leaves_no_trailing_garbage() {
        // The in-place write must truncate: shrinking from two entries to one
        // would otherwise leave half the old JSON dangling past the new.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("registry.json");
        register_at(&path, Path::new("/tmp/a-rather-long-project-path"));
        register_at(&path, Path::new("/tmp/b"));
        with_locked(&path, |registry| {
            registry.projects.retain(|p| p.ends_with("/b"));
            (true, ())
        })
        .unwrap();
        assert_eq!(read(&path).projects, ["/tmp/b"]);
    }
}
