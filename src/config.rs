//! Optional per-project configuration: `popgres.toml`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

pub const CONFIG_FILE: &str = "popgres.toml";

/// Everything a project can pin down in `popgres.toml`. Every field is optional;
/// a command-line flag always wins over the file.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Postgres version requirement, e.g. "18" or "=18.4.0".
    pub pg_version: Option<String>,
    /// Database created on first start (default: "db").
    pub database: Option<String>,
    /// Password for the superuser. Left out, the instance takes no password at all.
    pub password: Option<String>,
    /// Fixed port. 0 means "pick a free one", same as leaving it out.
    pub port: Option<u16>,
    /// Persist the data directory between runs.
    pub keep: Option<bool>,
    /// SQL file or shell command to run once, after a fresh initdb.
    pub seed: Option<String>,
    /// How long an instance may live before `popgres gc` disposes of it,
    /// e.g. "30m". Left out, it lives until someone stops it.
    pub ttl: Option<String>,
    /// File to write DATABASE_URL into when an instance starts.
    pub env_file: Option<PathBuf>,
    /// Where the instance lives: "local" (`.popgres/` in the project, the
    /// default) or "global" (the per-user data directory, keyed by path).
    pub location: Option<Location>,
    /// PostgreSQL extensions to install and create, e.g. ["vector"].
    pub extensions: Option<Vec<String>>,
    /// Optional version pins for entries in `extensions`,
    /// e.g. vector = "=0.8.0". Unpinned extensions take the latest build.
    pub extensions_versions: Option<std::collections::BTreeMap<String, String>>,
}

/// Where a project's database lives.
///
/// Local is the default: the instance sits in `.popgres/` inside the project,
/// so deleting the project deletes its database. Global keeps the project
/// tree free of database files — the right choice for project directories
/// that live in synced folders (Dropbox, iCloud), where a live data
/// directory being synced risks corruption.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Location {
    #[default]
    Local,
    Global,
}

impl Config {
    fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        toml::from_str(&raw).with_context(|| format!("cannot parse {}", path.display()))
    }

    /// A fixed port, treating the documented `port = 0` as "pick a free one".
    pub fn fixed_port(&self) -> Option<u16> {
        self.port.filter(|port| *port != 0)
    }

    pub fn resolved_location(&self) -> Location {
        self.location.unwrap_or_default()
    }
}

/// Parse a TTL like `90s`, `30m`, `2h`, `1d`. A bare number means seconds.
pub fn parse_ttl(raw: &str) -> Result<std::time::Duration> {
    let raw = raw.trim();
    let (digits, unit_seconds) = match raw.chars().last() {
        Some('s') => (&raw[..raw.len() - 1], 1),
        Some('m') => (&raw[..raw.len() - 1], 60),
        Some('h') => (&raw[..raw.len() - 1], 60 * 60),
        Some('d') => (&raw[..raw.len() - 1], 24 * 60 * 60),
        _ => (raw, 1),
    };
    let amount: u64 = digits.trim().parse().map_err(|_| {
        anyhow::anyhow!("invalid ttl `{raw}` — use a number with s, m, h or d, e.g. `30m`")
    })?;
    if amount == 0 {
        anyhow::bail!("invalid ttl `{raw}` — a ttl must be longer than zero");
    }
    let seconds = amount.checked_mul(unit_seconds).ok_or_else(|| {
        anyhow::anyhow!("invalid ttl `{raw}` — the requested duration is too large")
    })?;
    Ok(std::time::Duration::from_secs(seconds))
}

/// Find the project this directory belongs to: the nearest ancestor holding a
/// `popgres.toml`, else the nearest git root, else the directory itself.
///
/// Keying off the project root rather than the current directory means `popgres
/// up` in the repo root and `popgres url` in a subdirectory find the same instance.
/// The config of a known project root, without walking ancestors.
pub fn at(root: &Path) -> Result<Config> {
    let config_file = root.join(CONFIG_FILE);
    if config_file.is_file() {
        Config::load(&config_file)
    } else {
        Ok(Config::default())
    }
}

pub fn discover(start: &Path) -> Result<(PathBuf, Config)> {
    for dir in start.ancestors() {
        let config_file = dir.join(CONFIG_FILE);
        if config_file.is_file() {
            return Ok((dir.to_path_buf(), Config::load(&config_file)?));
        }
        if dir.join(".git").exists() {
            return Ok((dir.to_path_buf(), Config::default()));
        }
    }
    Ok((start.to_path_buf(), Config::default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_full_config() {
        let config: Config = toml::from_str(
            r#"
            pg_version = "18"
            database = "shop"
            password = "hunter2"
            port = 5544
            keep = true
            ttl = "30m"
            seed = "./db/seed.sql"
            env_file = ".env.local"
            "#,
        )
        .unwrap();

        assert_eq!(config.pg_version.as_deref(), Some("18"));
        assert_eq!(config.database.as_deref(), Some("shop"));
        assert_eq!(config.password.as_deref(), Some("hunter2"));
        assert_eq!(config.fixed_port(), Some(5544));
        assert_eq!(config.keep, Some(true));
        assert_eq!(config.ttl.as_deref(), Some("30m"));
        assert_eq!(config.seed.as_deref(), Some("./db/seed.sql"));
        assert_eq!(config.env_file, Some(PathBuf::from(".env.local")));
    }

    #[test]
    fn an_empty_config_is_all_defaults() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.pg_version.is_none());
        assert_eq!(config.fixed_port(), None);
        // No password configured is what gives an instance no password at all.
        assert!(config.password.is_none());
    }

    #[test]
    fn ttl_units_all_parse() {
        assert_eq!(parse_ttl("90s").unwrap().as_secs(), 90);
        assert_eq!(parse_ttl("30m").unwrap().as_secs(), 1800);
        assert_eq!(parse_ttl("2h").unwrap().as_secs(), 7200);
        assert_eq!(parse_ttl("1d").unwrap().as_secs(), 86400);
        // A bare number is seconds, and surrounding space is forgiven.
        assert_eq!(parse_ttl(" 45 ").unwrap().as_secs(), 45);
    }

    #[test]
    fn a_nonsense_ttl_is_rejected_with_guidance() {
        for bad in ["", "soon", "30x", "-5m", "0", "0h", "18446744073709551615d"] {
            let error = parse_ttl(bad).unwrap_err().to_string();
            assert!(error.contains("ttl"), "{bad} produced: {error}");
        }
    }

    #[test]
    fn ttl_can_come_from_the_config_file() {
        let config: Config = toml::from_str(r#"ttl = "30m""#).unwrap();
        assert_eq!(config.ttl.as_deref(), Some("30m"));
        assert_eq!(
            parse_ttl(config.ttl.as_deref().unwrap()).unwrap().as_secs(),
            1800
        );
    }

    #[test]
    fn port_zero_means_pick_a_free_one() {
        let config: Config = toml::from_str("port = 0").unwrap();
        assert_eq!(config.fixed_port(), None);
    }

    #[test]
    fn a_misspelled_key_is_an_error_rather_than_silence() {
        let result: Result<Config, _> = toml::from_str(r#"pg_verison = "18""#);
        assert!(result.is_err());
    }

    #[test]
    fn popgres_toml_marks_the_project_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let nested = root.join("services/api");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(root.join(CONFIG_FILE), r#"database = "shop""#).unwrap();

        let (found, config) = discover(&nested).unwrap();
        assert_eq!(found, root);
        assert_eq!(config.database.as_deref(), Some("shop"));
    }

    #[test]
    fn a_git_root_marks_the_project_when_there_is_no_config() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let nested = root.join("crates/inner");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();

        let (found, config) = discover(&nested).unwrap();
        assert_eq!(found, root);
        assert!(config.database.is_none());
    }

    #[test]
    fn the_nearest_marker_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let inner = root.join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(inner.join(CONFIG_FILE), "").unwrap();

        assert_eq!(discover(&inner).unwrap().0, inner);
    }

    #[test]
    fn an_unmarked_directory_is_its_own_project() {
        let tmp = tempfile::tempdir().unwrap();
        let plain = tmp.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();

        // No popgres.toml and no .git anywhere above it in the temp tree.
        let (found, _) = discover(&plain).unwrap();
        assert!(found.starts_with(tmp.path()));
    }
}
