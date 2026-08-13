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
    /// File to write DATABASE_URL into when an instance starts.
    pub env_file: Option<PathBuf>,
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
}

/// Find the project this directory belongs to: the nearest ancestor holding a
/// `popgres.toml`, else the nearest git root, else the directory itself.
///
/// Keying off the project root rather than the current directory means `popgres
/// up` in the repo root and `popgres url` in a subdirectory find the same instance.
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
