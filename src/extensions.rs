//! Extension variants: immutable installation folders in the global store.
//!
//! The base PostgreSQL install is never written after download — the pgvector
//! spike proved why: installing an extension into the shared install silently
//! installs it for every project on that version. Instead, a project that
//! configures extensions gets a *variant*: a copy-on-write clone of the base
//! with its extensions installed, stored globally under a key derived from
//! the resolved versions (`16.14.0+vector@0.8.0`), built once and shared
//! read-only by every project that wants the same combination.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::commands::emit_event;
use crate::config::Config;

/// Our inventory inside a variant folder. postgresql_extensions keeps its own
/// records; this one is popgres's contract — what the folder was built from.
pub const MANIFEST_FILE: &str = "popgres-manifest.json";

const VARIANTS_DIR: &str = "variants";
/// A variant untouched this recently is never evicted: it may belong to an
/// instance whose state file is seconds away from being written.
const EVICT_MIN_AGE_SECS: u64 = 3600;

/// One configured extension, resolved to its source.
#[derive(Debug, Clone)]
pub struct ExtensionSpec {
    /// The name the user configured and `CREATE EXTENSION` uses.
    pub name: &'static str,
    pub namespace: &'static str,
    pub repository: &'static str,
    /// The SQL-level extension name (differs from `name` for pgvecto.rs).
    pub create_as: &'static str,
    /// Requirement from config; "*" when unpinned.
    pub requirement: String,
    /// Shown when no build exists for the chosen PostgreSQL.
    hint: &'static str,
}

struct KnownExtension {
    aliases: &'static [&'static str],
    name: &'static str,
    namespace: &'static str,
    repository: &'static str,
    create_as: &'static str,
    hint: &'static str,
}

/// The curated map from friendly names to sources. Small on purpose: every
/// entry here is something popgres has actually verified end to end.
const KNOWN: &[KnownExtension] = &[
    KnownExtension {
        aliases: &["vector", "pgvector"],
        name: "vector",
        namespace: "portal-corp",
        repository: "pgvector_compiled",
        create_as: "vector",
        hint: "pgvector currently ships prebuilt for PostgreSQL 16 — set `pg_version = \"16\"` in popgres.toml",
    },
    KnownExtension {
        aliases: &["vectors", "pgvecto.rs", "pgvecto-rs"],
        name: "vectors",
        namespace: "tensor-chord",
        repository: "pgvecto.rs",
        create_as: "vectors",
        hint: "pgvecto.rs ships prebuilt for specific PostgreSQL majors; try a different `pg_version`",
    },
];

/// Resolve the configured `extensions` (and version pins) against the curated
/// map, or explain what popgres knows how to install.
pub fn specs(config: &Config) -> Result<Vec<ExtensionSpec>> {
    let names = config.extensions.clone().unwrap_or_default();
    let pins = config.extensions_versions.clone().unwrap_or_default();

    let mut specs = Vec::new();
    for raw in &names {
        let Some(known) = KNOWN
            .iter()
            .find(|known| known.aliases.contains(&raw.to_lowercase().as_str()))
        else {
            let available: Vec<_> = KNOWN.iter().map(|known| known.name).collect();
            bail!(
                "unknown extension `{raw}` — popgres can install: {}",
                available.join(", ")
            );
        };
        let requirement = pins
            .get(raw)
            .or_else(|| pins.get(known.name))
            .cloned()
            .unwrap_or_else(|| "*".to_string());
        semver::VersionReq::parse(&requirement)
            .with_context(|| format!("invalid version for extension `{raw}`: {requirement}"))?;
        specs.push(ExtensionSpec {
            name: known.name,
            namespace: known.namespace,
            repository: known.repository,
            create_as: known.create_as,
            requirement,
            hint: known.hint,
        });
    }
    specs.sort_by_key(|spec| spec.name);
    specs.dedup_by_key(|spec| spec.name);

    for pinned in pins.keys() {
        if !specs.iter().any(|spec| {
            spec.name == pinned
                || KNOWN
                    .iter()
                    .any(|k| k.name == spec.name && k.aliases.contains(&pinned.as_str()))
        }) {
            bail!("extensions_versions pins `{pinned}`, but it is not listed in `extensions`");
        }
    }
    Ok(specs)
}

/// What a variant folder was built from.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    pub pg_version: String,
    /// name → resolved version, e.g. { "vector": "0.8.0" }.
    pub extensions: std::collections::BTreeMap<String, String>,
}

impl Manifest {
    pub fn read(variant_dir: &Path) -> Result<Self> {
        let path = variant_dir.join(MANIFEST_FILE);
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        serde_json::from_str(&raw).with_context(|| format!("corrupt manifest {}", path.display()))
    }

    /// Whether this variant satisfies the configured specs: same PostgreSQL,
    /// exactly these extensions, every pin honored. Unpinned requirements
    /// accept whatever the variant holds — re-resolving "latest" on every
    /// start would defeat the offline cache.
    pub fn satisfies(&self, pg_version: &str, specs: &[ExtensionSpec]) -> bool {
        if self.pg_version != pg_version || self.extensions.len() != specs.len() {
            return false;
        }
        specs.iter().all(|spec| {
            let Some(version) = self.extensions.get(spec.name) else {
                return false;
            };
            let Ok(version) = semver::Version::parse(version) else {
                return false;
            };
            semver::VersionReq::parse(&spec.requirement)
                .map(|requirement| requirement.matches(&version))
                .unwrap_or(false)
        })
    }

    /// The store key: `16.14.0+vector@0.8.0`, extensions sorted by name.
    pub fn key(&self) -> String {
        let mut key = self.pg_version.clone();
        for (name, version) in &self.extensions {
            key.push_str(&format!("+{name}@{version}"));
        }
        key
    }
}

pub fn variants_root() -> Result<PathBuf> {
    Ok(crate::state::popgres_data_dir()?.join(VARIANTS_DIR))
}

/// The installation directory to run this project from: an existing variant
/// that satisfies the specs, or a freshly built one.
///
/// `base` is the resolved, pristine install (never written). The build path
/// clones it, installs the extensions into the clone, and atomically renames
/// the clone into the store — losing the rename race to a concurrent builder
/// just means using their identical result.
pub async fn ensure_variant(
    base: &postgresql_embedded::Settings,
    specs: &[ExtensionSpec],
    json: bool,
) -> Result<PathBuf> {
    let pg_version = base
        .installation_dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| base.version.to_string());
    let root = variants_root()?;
    std::fs::create_dir_all(&root).with_context(|| format!("cannot create {}", root.display()))?;

    // Reuse before building: any variant satisfying the specs will do.
    for entry in std::fs::read_dir(&root)? {
        let path = entry?.path();
        if !path.is_dir() || is_temp(&path) {
            continue;
        }
        if let Ok(manifest) = Manifest::read(&path) {
            if manifest.satisfies(&pg_version, specs) {
                return Ok(path);
            }
        }
    }

    let names: Vec<_> = specs.iter().map(|spec| spec.name).collect();
    emit_event(
        json,
        serde_json::json!({
            "event": "installing_extensions",
            "pg_version": pg_version,
            "extensions": names,
        }),
        &format!(
            "popgres: installing {} for PostgreSQL {pg_version} (built once, shared by every project)...",
            names.join(", ")
        ),
    );

    let temp = root.join(format!(
        ".tmp-{}-{}",
        std::process::id(),
        rand::random::<u32>()
    ));
    let built = build_variant(base, specs, &pg_version, &temp).await;
    match built {
        Ok(manifest) => {
            let target = root.join(manifest.key());
            match std::fs::rename(&temp, &target) {
                Ok(()) => Ok(target),
                // A concurrent builder won the race; their variant is
                // identical by construction.
                Err(_) if target.exists() => {
                    std::fs::remove_dir_all(&temp).ok();
                    Ok(target)
                }
                Err(error) => {
                    std::fs::remove_dir_all(&temp).ok();
                    Err(error)
                        .with_context(|| format!("cannot move variant to {}", target.display()))
                }
            }
        }
        Err(error) => {
            std::fs::remove_dir_all(&temp).ok();
            Err(error)
        }
    }
}

async fn build_variant(
    base: &postgresql_embedded::Settings,
    specs: &[ExtensionSpec],
    pg_version: &str,
    temp: &Path,
) -> Result<Manifest> {
    clone_dir(&base.installation_dir, temp).with_context(|| {
        format!(
            "cannot clone {} into the variant store",
            base.installation_dir.display()
        )
    })?;

    let mut settings = base.clone();
    settings.installation_dir = temp.to_path_buf();
    for spec in specs {
        let requirement = semver::VersionReq::parse(&spec.requirement)?;
        postgresql_extensions::install(&settings, spec.namespace, spec.repository, &requirement)
            .await
            .with_context(|| {
                format!(
                    "extension `{}` ({}/{}) has no usable build for PostgreSQL {pg_version}; {}",
                    spec.name, spec.namespace, spec.repository, spec.hint
                )
            })?;
    }

    // Record what actually got installed — resolved versions, not requirements.
    let installed = postgresql_extensions::get_installed_extensions(&settings)
        .await
        .context("cannot read back the installed extensions")?;
    let mut extensions = std::collections::BTreeMap::new();
    for spec in specs {
        let Some(found) = installed.iter().find(|installed| {
            installed.namespace() == spec.namespace && installed.name() == spec.repository
        }) else {
            bail!(
                "extension `{}` did not register after installation — the variant is incomplete",
                spec.name
            );
        };
        extensions.insert(spec.name.to_string(), found.version().to_string());
    }
    let manifest = Manifest {
        pg_version: pg_version.to_string(),
        extensions,
    };
    std::fs::write(
        temp.join(MANIFEST_FILE),
        serde_json::to_string_pretty(&manifest)?,
    )
    .with_context(|| format!("cannot write {}", temp.join(MANIFEST_FILE).display()))?;
    Ok(manifest)
}

/// Create the configured extensions in the instance's database, so the seed
/// hook (and everything after it) finds them ready.
pub fn create_in_database(
    state: &crate::state::InstanceState,
    specs: &[ExtensionSpec],
) -> Result<()> {
    let psql = crate::instance::psql_binary(state)?;
    for spec in specs {
        let sql = format!("CREATE EXTENSION IF NOT EXISTS \"{}\"", spec.create_as);
        let output = std::process::Command::new(&psql)
            .arg(state.url())
            .args([
                "--quiet",
                "--no-psqlrc",
                "-v",
                "ON_ERROR_STOP=1",
                "-c",
                &sql,
            ])
            .output()
            .with_context(|| format!("failed to run {}", psql.display()))?;
        if !output.status.success() {
            bail!(
                "failed to create extension `{}`: {}",
                spec.create_as,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    }
    Ok(())
}

/// A kept data directory was initdb'd against one set of extensions; its
/// shared libraries are baked into the catalog. Changing the configuration
/// under it needs a rebuild, and saying so beats a postmaster startup error.
pub fn check_resume_extensions(installation_dir: &str, specs: &[ExtensionSpec]) -> Result<()> {
    let manifest = Manifest::read(Path::new(installation_dir)).ok();
    let had: Vec<String> = manifest
        .map(|manifest| manifest.extensions.keys().cloned().collect())
        .unwrap_or_default();
    let now: Vec<String> = specs.iter().map(|spec| spec.name.to_string()).collect();
    if had != now {
        bail!(
            "the extension configuration changed for a kept database (was [{}], now [{}]) — run `popgres reset` to rebuild with the new extensions",
            had.join(", "),
            now.join(", ")
        );
    }
    Ok(())
}

/// Remove variants no known instance references. Variants are cache: without
/// eviction every abandoned combination costs ~43 MB forever.
pub fn evict_unreferenced(
    referenced_installation_dirs: &[PathBuf],
    dry_run: bool,
) -> Result<Vec<String>> {
    let root = variants_root()?;
    if !root.exists() {
        return Ok(Vec::new());
    }
    evict_in(
        &root,
        referenced_installation_dirs,
        dry_run,
        crate::state::now_unix(),
    )
}

fn evict_in(
    root: &Path,
    referenced_installation_dirs: &[PathBuf],
    dry_run: bool,
    now: u64,
) -> Result<Vec<String>> {
    let mut evicted = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path();
        if !path.is_dir() {
            continue;
        }
        let age = std::fs::metadata(&path)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |mtime| now.saturating_sub(mtime.as_secs()));
        if is_temp(&path) {
            // A crashed build's leftovers; give a live builder a wide berth.
            if age > 24 * 3600 && !dry_run {
                std::fs::remove_dir_all(&path).ok();
            }
            continue;
        }
        let referenced = referenced_installation_dirs
            .iter()
            .any(|installation| installation == &path);
        if referenced || age < EVICT_MIN_AGE_SECS {
            continue;
        }
        if !dry_run {
            std::fs::remove_dir_all(&path)
                .with_context(|| format!("cannot remove {}", path.display()))?;
        }
        evicted.push(
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
        );
    }
    evicted.sort();
    Ok(evicted)
}

fn is_temp(path: &Path) -> bool {
    path.file_name()
        .map(|name| name.to_string_lossy().starts_with(".tmp-"))
        .unwrap_or(false)
}

/// Clone a directory tree, using the filesystem's copy-on-write when it has
/// one (measured: the whole 43 MB install in 0.24 s and ~zero real disk on
/// APFS) and falling back to a plain recursive copy.
fn clone_dir(source: &Path, target: &Path) -> Result<()> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let mut command = std::process::Command::new("cp");
        if cfg!(target_os = "macos") {
            command.arg("-Rc"); // APFS clonefile
        } else {
            command.args(["-R", "--reflink=auto"]); // btrfs/XFS reflink
        }
        if let Ok(status) = command.arg(source).arg(target).status() {
            if status.success() {
                return Ok(());
            }
        }
        std::fs::remove_dir_all(target).ok();
    }
    copy_recursive(source, target)
}

fn copy_recursive(source: &Path, target: &Path) -> Result<()> {
    std::fs::create_dir_all(target)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let from = entry.path();
        let to = target.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_recursive(&from, &to)?;
        } else if kind.is_symlink() {
            #[cfg(unix)]
            {
                let link = std::fs::read_link(&from)?;
                std::os::unix::fs::symlink(&link, &to)?;
            }
            #[cfg(windows)]
            std::fs::copy(&from, &to).map(|_| ())?;
        } else {
            // fs::copy preserves permissions on unix — the bin/ tree stays
            // executable.
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn config(toml: &str) -> Config {
        toml::from_str(toml).unwrap()
    }

    #[test]
    fn vector_resolves_to_its_portal_corp_source() {
        let specs = specs(&config(r#"extensions = ["vector"]"#)).unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].namespace, "portal-corp");
        assert_eq!(specs[0].repository, "pgvector_compiled");
        assert_eq!(specs[0].create_as, "vector");
        assert_eq!(specs[0].requirement, "*");
    }

    #[test]
    fn the_pgvector_alias_and_duplicates_collapse() {
        let specs = specs(&config(r#"extensions = ["pgvector", "vector"]"#)).unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "vector");
    }

    #[test]
    fn an_unknown_extension_lists_what_is_available() {
        let error = specs(&config(r#"extensions = ["postgis"]"#)).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("postgis"));
        assert!(message.contains("vector"));
    }

    #[test]
    fn a_version_pin_is_carried_and_validated() {
        let specs = specs(&config(
            "extensions = [\"vector\"]\n[extensions_versions]\nvector = \"=0.8.0\"",
        ))
        .unwrap();
        assert_eq!(specs[0].requirement, "=0.8.0");

        assert!(
            specs_err("extensions = [\"vector\"]\n[extensions_versions]\nvector = \"latest\"")
                .contains("invalid version")
        );
    }

    #[test]
    fn a_pin_for_an_unlisted_extension_is_an_error() {
        assert!(
            specs_err("extensions = []\n[extensions_versions]\nvector = \"=0.8.0\"")
                .contains("not listed in `extensions`")
        );
    }

    fn specs_err(raw: &str) -> String {
        specs(&config(raw)).unwrap_err().to_string()
    }

    #[test]
    fn the_variant_key_is_version_plus_sorted_extensions() {
        let manifest = Manifest {
            pg_version: "16.14.0".to_string(),
            extensions: [
                ("vectors".to_string(), "0.4.0".to_string()),
                ("vector".to_string(), "0.8.0".to_string()),
            ]
            .into(),
        };
        assert_eq!(manifest.key(), "16.14.0+vector@0.8.0+vectors@0.4.0");
    }

    #[test]
    fn a_manifest_satisfies_matching_specs_and_pins() {
        let manifest = Manifest {
            pg_version: "16.14.0".to_string(),
            extensions: [("vector".to_string(), "0.8.0".to_string())].into(),
        };
        let unpinned = specs(&config(r#"extensions = ["vector"]"#)).unwrap();
        assert!(manifest.satisfies("16.14.0", &unpinned));
        assert!(!manifest.satisfies("18.4.0", &unpinned));

        let pinned = specs(&config(
            "extensions = [\"vector\"]\n[extensions_versions]\nvector = \"=0.7.0\"",
        ))
        .unwrap();
        assert!(!manifest.satisfies("16.14.0", &pinned));

        let empty: Vec<ExtensionSpec> = Vec::new();
        assert!(!manifest.satisfies("16.14.0", &empty));
    }

    #[test]
    fn resuming_with_changed_extensions_asks_for_a_reset() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = Manifest {
            pg_version: "16.14.0".to_string(),
            extensions: [("vector".to_string(), "0.8.0".to_string())].into(),
        };
        std::fs::write(
            dir.path().join(MANIFEST_FILE),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();
        let installation = dir.path().display().to_string();

        let same = specs(&config(r#"extensions = ["vector"]"#)).unwrap();
        check_resume_extensions(&installation, &same).unwrap();

        let none: Vec<ExtensionSpec> = Vec::new();
        let error = check_resume_extensions(&installation, &none).unwrap_err();
        assert!(error.to_string().contains("popgres reset"));

        // A plain base install (no manifest) with no configured extensions is
        // the normal case and must pass.
        let plain = tempfile::tempdir().unwrap();
        check_resume_extensions(&plain.path().display().to_string(), &none).unwrap();
    }

    #[test]
    fn the_recursive_copy_preserves_structure_and_permissions() {
        let source = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(source.path().join("bin")).unwrap();
        std::fs::write(source.path().join("bin/tool"), "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                source.path().join("bin/tool"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
        let target = tempfile::tempdir().unwrap();
        let target = target.path().join("clone");

        copy_recursive(source.path(), &target).unwrap();

        assert!(target.join("bin/tool").is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(target.join("bin/tool"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "executable bit must survive");
        }
    }

    #[test]
    fn eviction_spares_referenced_and_recent_variants() {
        // Uses a fake variants layout through the public fn by pointing refs
        // at real paths; age is the gate we cannot fake, so only the
        // referenced/temp classification is asserted here.
        let root = tempfile::tempdir().unwrap();
        let variant = root.path().join("16.14.0+vector@0.8.0");
        std::fs::create_dir_all(&variant).unwrap();
        // Freshly created → under the age floor → spared even when
        // unreferenced, dry-run or not.
        let evicted = super::evict_in(root.path(), &[], true, crate::state::now_unix()).unwrap();
        assert!(evicted.is_empty());

        // Backdate past the floor: now it is reported…
        let old = crate::state::now_unix() + EVICT_MIN_AGE_SECS + 10;
        let evicted = super::evict_in(root.path(), &[], true, old).unwrap();
        assert_eq!(evicted, ["16.14.0+vector@0.8.0"]);
        assert!(variant.exists(), "a dry run must not delete");

        // …unless referenced.
        let evicted =
            super::evict_in(root.path(), std::slice::from_ref(&variant), true, old).unwrap();
        assert!(evicted.is_empty());

        // A real run removes it.
        let evicted = super::evict_in(root.path(), &[], false, old).unwrap();
        assert_eq!(evicted, ["16.14.0+vector@0.8.0"]);
        assert!(!variant.exists());
    }
}
