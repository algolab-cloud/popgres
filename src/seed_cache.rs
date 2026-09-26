//! Ready-made data directories.
//!
//! initdb alone is most of a fresh start, and a seed can take far longer. So
//! once a fresh instance is initialized and seeded, popgres keeps a copy of
//! its stopped data directory, keyed by everything that shaped it: the
//! PostgreSQL install, extensions, credentials, server settings, and the
//! seed with its inputs. The next fresh start with the same key copies that
//! directory into place — no initdb, no seed.
//!
//! The key must change whenever the result could, so a cache hit is never
//! stale. A `.sql` seed is hashed by content; a shell-command seed depends
//! on files popgres cannot guess, so it is only cached when `seed_inputs`
//! lists them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::project::Project;

const SEEDS_DIR: &str = "seeds";
const MANIFEST_FILE: &str = "popgres-seed.json";
/// Entries kept per project; branch switching between a few seed versions
/// stays fast without every old seed piling up.
const KEEP_PER_PROJECT: usize = 3;
/// `gc` evicts an unreferenced entry unused for this long.
const EVICT_AFTER_SECS: u64 = 7 * 24 * 3600;
/// `cache --clean` spares entries used this recently.
const RECENT_SECS: u64 = 3600;
/// Nothing removes an entry used this recently: a start may be copying it.
/// Short, so an actively edited seed does not pile up an entry per edit.
const COPY_GUARD_SECS: u64 = 300;

/// What a cached data directory records about itself.
#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub project_dir: String,
    pub pg_version: String,
    pub created_at: u64,
}

/// Everything about an instance, besides the project's seed, that ends up
/// in its data directory.
pub struct Fingerprint<'a> {
    pub installation_dir: &'a Path,
    pub pg_version: &'a str,
    pub database: &'a str,
    pub username: &'a str,
    /// The configured password; `None` for the default trust auth.
    pub password: Option<&'a str>,
    pub extensions: &'a [String],
    pub settings: &'a BTreeMap<String, String>,
}

/// The cache key for a fresh instance of `project`, or `None` when it must
/// not be cached: caching is off, or the seed is a command whose inputs
/// are unknown.
pub fn key(project: &Project, fingerprint: &Fingerprint) -> Result<Option<String>> {
    let config = &project.config;
    if config.seed_cache == Some(false) {
        return Ok(None);
    }
    let mut hasher = Sha256::new();
    let mut feed = |label: &str, bytes: &[u8]| {
        hasher.update(label.as_bytes());
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    };
    // The layout popgres builds (template, locking) is part of the result.
    feed("popgres", env!("CARGO_PKG_VERSION").as_bytes());
    feed(
        "installation",
        fingerprint.installation_dir.to_string_lossy().as_bytes(),
    );
    feed("pg_version", fingerprint.pg_version.as_bytes());
    feed("database", fingerprint.database.as_bytes());
    feed("username", fingerprint.username.as_bytes());
    match fingerprint.password {
        Some(password) => feed("password", password.as_bytes()),
        None => feed("trust", b""),
    }
    for extension in fingerprint.extensions {
        feed("extension", extension.as_bytes());
    }
    for (name, value) in fingerprint.settings {
        feed("setting", format!("{name}={value}").as_bytes());
    }

    if let Some(recipe) = config.seed.as_deref() {
        feed("seed", recipe.as_bytes());
        match crate::seed::sql_file(project, recipe) {
            Some(path) => feed_file(&mut feed, &project.root, &path)?,
            None if config.seed_inputs.is_none() => return Ok(None),
            None => {}
        }
    }
    for input in config.seed_inputs.iter().flatten() {
        let path = project.root.join(input);
        if !path.exists() {
            bail!("seed_inputs lists {}, which does not exist", path.display());
        }
        feed_tree(&mut feed, &project.root, &path)?;
    }
    Ok(Some(crate::state::hex(&hasher.finalize()[..16])))
}

fn feed_tree(feed: &mut impl FnMut(&str, &[u8]), root: &Path, path: &Path) -> Result<()> {
    if !path.is_dir() {
        return feed_file(feed, root, path);
    }
    let mut entries: Vec<PathBuf> = std::fs::read_dir(path)
        .with_context(|| format!("cannot read {}", path.display()))?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<_>>()
        .with_context(|| format!("cannot read {}", path.display()))?;
    entries.sort();
    for entry in entries {
        feed_tree(feed, root, &entry)?;
    }
    Ok(())
}

fn feed_file(feed: &mut impl FnMut(&str, &[u8]), root: &Path, path: &Path) -> Result<()> {
    let relative = path.strip_prefix(root).unwrap_or(path);
    // `/` on every platform, so a key never depends on the separator.
    let name: Vec<_> = relative
        .components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect();
    feed("path", name.join("/").as_bytes());
    let contents =
        std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    feed("contents", &contents);
    Ok(())
}

pub fn root() -> Result<PathBuf> {
    Ok(crate::state::popgres_data_dir()?.join(SEEDS_DIR))
}

/// Copy the cached data directory for `key` into `data_dir`. `false` when
/// there is none; on a failed copy `data_dir` is removed again.
pub fn restore(key: &str, data_dir: &Path) -> Result<bool> {
    let entry = root()?.join(key);
    let source = entry.join("data");
    if !source.join("postgresql.conf").is_file() {
        return Ok(false);
    }
    touch(&entry);
    if let Err(error) = crate::extensions::clone_dir(&source, data_dir) {
        std::fs::remove_dir_all(data_dir).ok();
        return Err(error).context("cannot copy the cached data directory");
    }
    // PostgreSQL refuses a data directory others can read, and a plain
    // recursive copy creates it with the default mode.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(data_dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("cannot restrict {}", data_dir.display()))?;
    }
    Ok(true)
}

/// Whether an entry for `key` is already stored.
pub fn contains(key: &str) -> Result<bool> {
    Ok(root()?.join(key).join(MANIFEST_FILE).is_file())
}

/// Store a *stopped* instance's data directory under `key`, then prune the
/// project's older entries. Losing a race to an identical concurrent store
/// is success.
pub fn store(key: &str, data_dir: &Path, manifest: &Manifest, referenced: &[String]) -> Result<()> {
    let root = root()?;
    std::fs::create_dir_all(&root).with_context(|| format!("cannot create {}", root.display()))?;
    let target = root.join(key);
    if target.join(MANIFEST_FILE).is_file() {
        touch(&target);
        return Ok(());
    }
    let temp = root.join(format!(
        ".tmp-{}-{}",
        std::process::id(),
        rand::random::<u32>()
    ));
    let built: Result<()> = (|| {
        std::fs::create_dir_all(&temp)?;
        crate::extensions::clone_dir(data_dir, &temp.join("data"))?;
        // The server's own start log is noise in a copy meant for others.
        std::fs::remove_file(temp.join("data").join("start.log")).ok();
        std::fs::write(
            temp.join(MANIFEST_FILE),
            serde_json::to_string_pretty(manifest)?,
        )?;
        Ok(())
    })();
    if let Err(error) = built {
        std::fs::remove_dir_all(&temp).ok();
        return Err(error).context("cannot store the seeded data directory");
    }
    if std::fs::rename(&temp, &target).is_err() {
        std::fs::remove_dir_all(&temp).ok();
        if !target.join(MANIFEST_FILE).is_file() {
            bail!(
                "cannot move the seeded data directory to {}",
                target.display()
            );
        }
    }
    prune_project(
        &root,
        &manifest.project_dir,
        referenced,
        crate::state::now_unix(),
    );
    Ok(())
}

/// One stored entry, for reports and eviction.
pub struct Entry {
    pub key: String,
    pub path: PathBuf,
    pub manifest: Option<Manifest>,
    /// Seconds since it was stored or last restored.
    pub age: u64,
}

impl Entry {
    pub fn recently_used(&self) -> bool {
        self.age < RECENT_SECS
    }
}

/// Every stored entry, newest first. In-progress stores are skipped.
pub fn entries() -> Result<Vec<Entry>> {
    entries_in(&root()?, crate::state::now_unix())
}

fn entries_in(root: &Path, now: u64) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    if !root.exists() {
        return Ok(entries);
    }
    for entry in
        std::fs::read_dir(root).with_context(|| format!("cannot read {}", root.display()))?
    {
        let path = entry?.path();
        let key = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !path.is_dir() || key.starts_with(".tmp-") {
            continue;
        }
        let manifest_path = path.join(MANIFEST_FILE);
        let manifest = std::fs::read_to_string(&manifest_path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok());
        entries.push(Entry {
            age: age_secs(&manifest_path, now),
            key,
            path,
            manifest,
        });
    }
    entries.sort_by_key(|entry| entry.age);
    Ok(entries)
}

/// Remove what `gc` should: unreferenced entries unused for a week, and
/// abandoned in-progress stores. Returns the keys removed (or that would be).
pub fn evict(referenced: &[String], dry_run: bool) -> Result<Vec<String>> {
    evict_in(&root()?, referenced, dry_run, crate::state::now_unix())
}

fn evict_in(root: &Path, referenced: &[String], dry_run: bool, now: u64) -> Result<Vec<String>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path();
        let is_temp = path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with(".tmp-"));
        if is_temp && age_secs(&path, now) > 24 * 3600 && !dry_run {
            std::fs::remove_dir_all(&path).ok();
        }
    }
    let mut evicted = Vec::new();
    for entry in entries_in(root, now)? {
        if referenced.contains(&entry.key) || entry.age < EVICT_AFTER_SECS {
            continue;
        }
        if !dry_run {
            std::fs::remove_dir_all(&entry.path)
                .with_context(|| format!("cannot remove {}", entry.path.display()))?;
        }
        evicted.push(entry.key);
    }
    Ok(evicted)
}

/// Keep a project's few most recently used entries (plus anything an
/// instance references); a changed seed otherwise leaves its predecessor
/// behind on every edit.
fn prune_project(root: &Path, project_dir: &str, referenced: &[String], now: u64) {
    let Ok(entries) = entries_in(root, now) else {
        return;
    };
    let stale = entries
        .into_iter()
        .filter(|entry| {
            entry
                .manifest
                .as_ref()
                .is_some_and(|manifest| manifest.project_dir == project_dir)
        })
        .skip(KEEP_PER_PROJECT)
        .filter(|entry| !referenced.contains(&entry.key) && entry.age >= COPY_GUARD_SECS);
    for entry in stale {
        std::fs::remove_dir_all(&entry.path).ok();
    }
}

fn touch(entry: &Path) {
    std::fs::File::options()
        .write(true)
        .open(entry.join(MANIFEST_FILE))
        .and_then(|file| file.set_modified(std::time::SystemTime::now()))
        .ok();
}

fn age_secs(path: &Path, now: u64) -> u64 {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |mtime| now.saturating_sub(mtime.as_secs()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn project(root: &Path, config: &str) -> Project {
        Project {
            root: root.to_path_buf(),
            state_dir: root.join(".popgres"),
            config: toml::from_str::<Config>(config).unwrap(),
            local: true,
        }
    }

    fn key_of(project: &Project) -> Option<String> {
        let settings = BTreeMap::new();
        key(
            project,
            &Fingerprint {
                installation_dir: Path::new("/cache/18.4.0"),
                pg_version: "18.4.0",
                database: "db",
                username: "postgres",
                password: None,
                extensions: &[],
                settings: &settings,
            },
        )
        .unwrap()
    }

    #[test]
    fn a_sql_seed_is_keyed_by_its_contents() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("seed.sql"), "create table a ();").unwrap();
        let project = project(dir.path(), r#"seed = "seed.sql""#);
        let first = key_of(&project).expect("a sql seed is cacheable");
        assert_eq!(key_of(&project).unwrap(), first, "keys are stable");

        std::fs::write(dir.path().join("seed.sql"), "create table b ();").unwrap();
        assert_ne!(key_of(&project).unwrap(), first, "an edit changes the key");
    }

    #[test]
    fn a_command_seed_is_only_cached_with_declared_inputs() {
        let dir = tempfile::tempdir().unwrap();
        assert!(key_of(&project(dir.path(), r#"seed = "npm run seed""#)).is_none());

        std::fs::create_dir_all(dir.path().join("migrations")).unwrap();
        std::fs::write(dir.path().join("migrations/001.sql"), "a").unwrap();
        let declared = project(
            dir.path(),
            "seed = \"npm run seed\"\nseed_inputs = [\"migrations\"]",
        );
        let first = key_of(&declared).expect("declared inputs make it cacheable");
        // A new migration anywhere under a listed directory changes the key.
        std::fs::write(dir.path().join("migrations/002.sql"), "b").unwrap();
        assert_ne!(key_of(&declared).unwrap(), first);
    }

    #[test]
    fn no_seed_is_cacheable_and_caching_can_be_turned_off() {
        let dir = tempfile::tempdir().unwrap();
        assert!(key_of(&project(dir.path(), "")).is_some());
        assert!(key_of(&project(dir.path(), "seed_cache = false")).is_none());
    }

    #[test]
    fn a_missing_seed_input_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let project = project(dir.path(), "seed_inputs = [\"nope\"]");
        let settings = BTreeMap::new();
        let error = key(
            &project,
            &Fingerprint {
                installation_dir: Path::new("/i"),
                pg_version: "18.4.0",
                database: "db",
                username: "postgres",
                password: None,
                extensions: &[],
                settings: &settings,
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("does not exist"));
    }

    #[test]
    fn settings_and_passwords_are_part_of_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let project = project(dir.path(), "");
        let empty = BTreeMap::new();
        let fast = BTreeMap::from([("fsync".to_string(), "off".to_string())]);
        let base = Fingerprint {
            installation_dir: Path::new("/i"),
            pg_version: "18.4.0",
            database: "db",
            username: "postgres",
            password: None,
            extensions: &[],
            settings: &empty,
        };
        let plain = key(&project, &base).unwrap();
        let with_settings = key(
            &project,
            &Fingerprint {
                settings: &fast,
                ..base
            },
        )
        .unwrap();
        let with_password = key(
            &project,
            &Fingerprint {
                password: Some("x"),
                settings: &empty,
                ..base
            },
        )
        .unwrap();
        assert_ne!(plain, with_settings);
        assert_ne!(plain, with_password);
    }

    #[test]
    fn entries_are_stored_restored_and_pruned_per_project() {
        let root = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("postgresql.conf"), "").unwrap();
        std::fs::write(source.path().join("start.log"), "noise").unwrap();

        let manifest = |project: &str| Manifest {
            project_dir: project.to_string(),
            pg_version: "18.4.0".to_string(),
            created_at: 0,
        };
        let store_in = |key: &str, project: &str| {
            let target = root.path().join(key);
            std::fs::create_dir_all(&target).unwrap();
            crate::extensions::clone_dir(source.path(), &target.join("data")).unwrap();
            std::fs::write(
                target.join(MANIFEST_FILE),
                serde_json::to_string(&manifest(project)).unwrap(),
            )
            .unwrap();
        };
        // Distinct ages, oldest first: entries stored within one second
        // would otherwise tie, and their order is up to the filesystem.
        let now = crate::state::now_unix();
        for (offset, key) in [40, 30, 20, 10].into_iter().zip(["a", "b", "c", "d"]) {
            store_in(key, "/p");
            std::fs::File::options()
                .write(true)
                .open(root.path().join(key).join(MANIFEST_FILE))
                .unwrap()
                .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(now - offset))
                .unwrap();
        }
        store_in("other", "/q");
        let keys = |at| -> Vec<String> {
            entries_in(root.path(), at)
                .unwrap()
                .into_iter()
                .map(|entry| entry.key)
                .collect()
        };

        // Everything is recent, so pruning spares it all…
        prune_project(root.path(), "/p", &[], now);
        assert_eq!(keys(now).len(), 5);
        // …and minutes on, a referenced entry still survives beyond the
        // newest three…
        let later = now + COPY_GUARD_SECS + 60;
        prune_project(root.path(), "/p", &["a".to_string()], later);
        assert_eq!(keys(later).len(), 5);
        // …but an unreferenced one goes, and other projects are untouched.
        prune_project(root.path(), "/p", &[], later);
        let left = keys(later);
        assert_eq!(left, ["other", "d", "c", "b"], "newest first");

        // A week on, gc evicts whatever nothing references.
        let week = crate::state::now_unix() + EVICT_AFTER_SECS + 10;
        let evicted = evict_in(root.path(), &["other".to_string()], true, week).unwrap();
        assert_eq!(evicted.len(), 3);
        assert!(!evicted.contains(&"other".to_string()));
    }
}
