//! `popgres cache`: what popgres keeps on disk, and reclaiming it.
//!
//! Three global pools grow over time — PostgreSQL base installs, extension
//! variants, and instance state — and until now only `gc`'s TTL path could
//! shrink any of them. The report shows each pool with what references it;
//! `--clean` removes what nothing references.
//!
//! Base installs need extra care: `~/.theseus/postgresql` is the shared cache
//! of every tool built on postgresql-embedded, not popgres's private
//! property. Removing a version popgres doesn't reference could still break
//! someone else's tooling, so bases are only touched behind `--all`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

/// One entry in a pool: something on disk with a size and a verdict.
#[derive(Debug, Serialize)]
pub struct PoolEntry {
    pub name: String,
    #[serde(skip)]
    pub path: PathBuf,
    pub size_bytes: u64,
    pub referenced: bool,
}

#[derive(Debug, Serialize)]
pub struct Report {
    /// PostgreSQL base installs under the shared download cache.
    pub postgres: Vec<PoolEntry>,
    /// Extension variants in popgres's own store.
    pub variants: Vec<PoolEntry>,
    /// Instance state directories (data, credentials, locks), local and global.
    pub instances: Vec<PoolEntry>,
    pub total_bytes: u64,
}

impl Report {
    pub fn gather() -> Result<Self> {
        let state_dirs = crate::instance::discoverable_state_dirs()?;
        let referenced: Vec<PathBuf> = state_dirs
            .iter()
            .filter_map(|dir| crate::state::InstanceState::load(dir).ok().flatten())
            .map(|state| PathBuf::from(state.installation_dir))
            .collect();

        let mut postgres = pool(&postgres_installs_root(), |path| {
            referenced.iter().any(|used| used == path)
        })?;
        // Only version-shaped directories are base installs; anything else in
        // the shared cache is not ours to report or touch.
        postgres.retain(|entry| semver::Version::parse(&entry.name).is_ok());

        let variants = pool(&crate::extensions::variants_root()?, |path| {
            referenced.iter().any(|used| used == path)
        })?;

        let instances = state_dirs
            .iter()
            .map(|dir| {
                let state = crate::state::InstanceState::load(dir).ok().flatten();
                PoolEntry {
                    name: state.map_or_else(
                        || dir.display().to_string(),
                        |state| state.project_dir.clone(),
                    ),
                    path: dir.clone(),
                    size_bytes: dir_size(dir),
                    // Instance state is never cache; it is the database.
                    referenced: true,
                }
            })
            .collect::<Vec<_>>();

        let total_bytes = postgres
            .iter()
            .chain(&variants)
            .chain(&instances)
            .map(|entry| entry.size_bytes)
            .sum();
        Ok(Self {
            postgres,
            variants,
            instances,
            total_bytes,
        })
    }

    /// Everything `--clean` would remove: unreferenced variants always,
    /// unreferenced base installs only with `--all`.
    pub fn removable(&self, all: bool) -> Vec<&PoolEntry> {
        self.variants
            .iter()
            .filter(|entry| !entry.referenced)
            .chain(
                self.postgres
                    .iter()
                    .filter(|entry| all && !entry.referenced),
            )
            .collect()
    }
}

/// Remove the removable set. Returns the names actually deleted.
pub fn clean(report: &Report, all: bool) -> Result<Vec<String>> {
    let mut removed = Vec::new();
    for entry in report.removable(all) {
        std::fs::remove_dir_all(&entry.path)
            .with_context(|| format!("cannot remove {}", entry.path.display()))?;
        removed.push(entry.name.clone());
    }
    Ok(removed)
}

/// The shared postgresql-embedded download cache (`~/.theseus/postgresql`).
fn postgres_installs_root() -> PathBuf {
    postgresql_embedded::Settings::default().installation_dir
}

fn pool(root: &Path, referenced: impl Fn(&Path) -> bool) -> Result<Vec<PoolEntry>> {
    let mut entries = Vec::new();
    if !root.exists() {
        return Ok(entries);
    }
    for entry in
        std::fs::read_dir(root).with_context(|| format!("cannot read {}", root.display()))?
    {
        let path = entry?.path();
        if !path.is_dir() {
            continue;
        }
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        // In-progress variant builds are not report material.
        if name.starts_with(".tmp-") {
            continue;
        }
        entries.push(PoolEntry {
            referenced: referenced(&path),
            size_bytes: dir_size(&path),
            name,
            path,
        });
    }
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(entries)
}

/// Apparent size of a tree. On copy-on-write filesystems a variant shares
/// its blocks with the base install, so summing pools overstates real disk —
/// the honest direction for a "how much could I get back" report.
fn dir_size(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .filter_map(|entry| entry.ok())
        .map(|entry| {
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => dir_size(&path),
                Ok(kind) if kind.is_file() => entry.metadata().map(|meta| meta.len()).unwrap_or(0),
                _ => 0,
            }
        })
        .sum()
}

/// `43.2 MB`-style sizes for the human report.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_like_a_human_wrote_them() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(999), "999 B");
        assert_eq!(human_size(43 * 1024 * 1024), "43.0 MB");
        assert_eq!(human_size(1024 * 1024 * 1024 + 1024 * 1024 * 200), "1.2 GB");
    }

    #[test]
    fn dir_size_sums_the_whole_tree() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
        std::fs::write(dir.path().join("a/one"), vec![0_u8; 100]).unwrap();
        std::fs::write(dir.path().join("a/b/two"), vec![0_u8; 50]).unwrap();
        assert_eq!(dir_size(dir.path()), 150);
    }

    #[test]
    fn a_pool_reports_dirs_with_reference_verdicts_and_skips_builds() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("used")).unwrap();
        std::fs::create_dir_all(root.path().join("unused")).unwrap();
        std::fs::create_dir_all(root.path().join(".tmp-123")).unwrap();
        std::fs::write(root.path().join("stray-file"), "x").unwrap();

        let used = root.path().join("used");
        let entries = pool(root.path(), |path| path == used).unwrap();

        let names: Vec<_> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, ["unused", "used"]);
        assert!(!entries[0].referenced);
        assert!(entries[1].referenced);
    }

    #[test]
    fn removable_takes_bases_only_with_all() {
        let report = Report {
            postgres: vec![entry("16.14.0", false), entry("18.4.0", true)],
            variants: vec![
                entry("16.14.0+vector@0.16.105", false),
                entry("18.4.0+vectors@0.4.0", true),
            ],
            instances: vec![],
            total_bytes: 0,
        };
        let names = |all| {
            report
                .removable(all)
                .iter()
                .map(|entry| entry.name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(false), ["16.14.0+vector@0.16.105"]);
        assert_eq!(names(true), ["16.14.0+vector@0.16.105", "16.14.0"]);
    }

    fn entry(name: &str, referenced: bool) -> PoolEntry {
        PoolEntry {
            name: name.to_string(),
            path: PathBuf::from("/nonexistent").join(name),
            size_bytes: 1,
            referenced,
        }
    }
}
