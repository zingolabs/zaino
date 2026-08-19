use std::collections::HashSet;
use std::path::Path;

use relman_core::types::{CrateName, ReleaseOptions, Target, WorkspacePath};
use serde::Deserialize;

use crate::config::ReleaseConfig;
use crate::error::ConfigError;

/// Default `[options]` values, applied when a key (or the whole table) is
/// absent. Kept next to the raw structs they fill in.
const DEFAULT_CHANGESETS_DIR: &str = ".changesets";
const DEFAULT_ROOT_MANIFEST: &str = "Cargo.toml";
const DEFAULT_WORKSPACE_CHANGELOG: &str = "CHANGELOG.md";
const CHANGELOG_BASENAME: &str = "CHANGELOG.md";

/// Load and parse a `relman.toml` into a typed [`ReleaseConfig`].
///
/// Reads the file, deserializes the raw schema, then converts into core
/// newtypes at the boundary — parsing strings, applying defaults, and
/// rejecting duplicate or empty target sets.
pub fn load(path: &Path) -> Result<ReleaseConfig, ConfigError> {
    let contents = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let raw: RawConfig = toml::from_str(&contents).map_err(|source| ConfigError::Toml {
        path: path.to_path_buf(),
        source,
    })?;
    raw.into_config()
}

/// The `relman.toml` document, mirrored for serde. `[[target]]` in TOML
/// deserializes as the `target` array.
#[derive(Debug, Deserialize)]
struct RawConfig {
    #[serde(default)]
    options: RawOptions,
    #[serde(default, rename = "target")]
    target: Vec<RawTarget>,
}

/// The `[options]` table; each key is optional and defaulted at conversion.
#[derive(Debug, Default, Deserialize)]
struct RawOptions {
    changesets_dir: Option<String>,
    root_manifest: Option<String>,
    workspace_changelog: Option<String>,
}

/// One `[[target]]` entry; `changelog`/`publish` are optional and defaulted.
#[derive(Debug, Deserialize)]
struct RawTarget {
    name: String,
    path: String,
    changelog: Option<String>,
    publish: Option<bool>,
}

impl RawConfig {
    fn into_config(self) -> Result<ReleaseConfig, ConfigError> {
        if self.target.is_empty() {
            return Err(ConfigError::NoTargets);
        }
        let options = self.options.into_options()?;

        let mut seen = HashSet::new();
        let mut targets = Vec::with_capacity(self.target.len());
        for raw in self.target {
            let target = raw.into_target()?;
            if !seen.insert(target.name().as_str().to_owned()) {
                return Err(ConfigError::DuplicateTarget(
                    target.name().as_str().to_owned(),
                ));
            }
            targets.push(target);
        }
        Ok(ReleaseConfig::new(options, targets))
    }
}

impl RawOptions {
    fn into_options(self) -> Result<ReleaseOptions, ConfigError> {
        let changesets_dir = parse_path(
            "options.changesets_dir",
            &self
                .changesets_dir
                .unwrap_or_else(|| DEFAULT_CHANGESETS_DIR.to_owned()),
        )?;
        let root_manifest = parse_path(
            "options.root_manifest",
            &self
                .root_manifest
                .unwrap_or_else(|| DEFAULT_ROOT_MANIFEST.to_owned()),
        )?;
        let workspace_changelog = parse_path(
            "options.workspace_changelog",
            &self
                .workspace_changelog
                .unwrap_or_else(|| DEFAULT_WORKSPACE_CHANGELOG.to_owned()),
        )?;
        Ok(ReleaseOptions::new(
            changesets_dir,
            root_manifest,
            workspace_changelog,
        ))
    }
}

impl RawTarget {
    fn into_target(self) -> Result<Target, ConfigError> {
        let name =
            CrateName::parse(&self.name).map_err(|source| ConfigError::InvalidCrateName {
                name: self.name.clone(),
                source,
            })?;
        let path = parse_path("target.path", &self.path)?;
        // Default: `<path>/CHANGELOG.md`. `path` is a validated relative path,
        // so joining a basename keeps it a valid workspace path.
        let changelog = match self.changelog {
            Some(raw) => parse_path("target.changelog", &raw)?,
            None => {
                let joined = path.as_path().join(CHANGELOG_BASENAME);
                let raw = joined.to_string_lossy();
                parse_path("target.changelog", &raw)?
            }
        };
        let publish = self.publish.unwrap_or(true);
        Ok(Target::new(name, path, changelog, publish))
    }
}

fn parse_path(field: &str, value: &str) -> Result<WorkspacePath, ConfigError> {
    WorkspacePath::parse(value).map_err(|source| ConfigError::InvalidPath {
        field: field.to_owned(),
        value: value.to_owned(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Write `contents` to a uniquely-named temp file and return its path.
    fn write_temp(contents: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir();
        let unique = format!(
            "relman-config-test-{}-{:?}.toml",
            std::process::id(),
            std::thread::current().id()
        );
        let path = dir.join(unique);
        let mut file = std::fs::File::create(&path).expect("create temp file");
        file.write_all(contents.as_bytes())
            .expect("write temp file");
        path
    }

    const VALID_WITH_OPTIONS: &str = r#"
[options]
changesets_dir      = ".changesets"
root_manifest       = "Cargo.toml"
workspace_changelog = "CHANGELOG.md"

[[target]]
name = "zaino-state"
path = "packages/zaino-state"

[[target]]
name = "zainod"
path = "packages/zainod"
changelog = "packages/zainod/CHANGES.md"
publish = false
"#;

    #[test]
    fn loads_targets_and_resolved_defaults() {
        let path = write_temp(VALID_WITH_OPTIONS);
        let config = load(&path).expect("valid config should load");
        std::fs::remove_file(&path).ok();

        assert_eq!(config.targets().len(), 2);

        let state = &config.targets()[0];
        assert_eq!(state.name().as_str(), "zaino-state");
        assert_eq!(state.path().as_str(), "packages/zaino-state");
        // changelog defaulted to <path>/CHANGELOG.md
        assert_eq!(
            state.changelog().as_str(),
            "packages/zaino-state/CHANGELOG.md"
        );
        // publish defaulted to true
        assert!(state.publish());

        let zainod = &config.targets()[1];
        // explicit overrides are honoured
        assert_eq!(zainod.changelog().as_str(), "packages/zainod/CHANGES.md");
        assert!(!zainod.publish());
    }

    #[test]
    fn options_default_when_table_omitted() {
        let toml = r#"
[[target]]
name = "zaino-state"
path = "packages/zaino-state"
"#;
        let path = write_temp(toml);
        let config = load(&path).expect("config without [options] should load");
        std::fs::remove_file(&path).ok();

        let options = config.options();
        assert_eq!(options.changesets_dir().as_str(), ".changesets");
        assert_eq!(options.root_manifest().as_str(), "Cargo.toml");
        assert_eq!(options.workspace_changelog().as_str(), "CHANGELOG.md");
    }

    #[test]
    fn rejects_invalid_crate_name() {
        let toml = r#"
[[target]]
name = "zaino.state"
path = "packages/zaino-state"
"#;
        let path = write_temp(toml);
        let err = load(&path).expect_err("invalid crate name should fail");
        std::fs::remove_file(&path).ok();
        assert!(matches!(err, ConfigError::InvalidCrateName { .. }));
    }

    #[test]
    fn rejects_duplicate_target_names() {
        let toml = r#"
[[target]]
name = "zaino-state"
path = "packages/zaino-state"

[[target]]
name = "zaino-state"
path = "packages/other"
"#;
        let path = write_temp(toml);
        let err = load(&path).expect_err("duplicate targets should fail");
        std::fs::remove_file(&path).ok();
        assert!(matches!(err, ConfigError::DuplicateTarget(name) if name == "zaino-state"));
    }

    #[test]
    fn rejects_empty_target_list() {
        let path = write_temp("[options]\n");
        let err = load(&path).expect_err("no targets should fail");
        std::fs::remove_file(&path).ok();
        assert!(matches!(err, ConfigError::NoTargets));
    }

    #[test]
    fn rejects_absolute_target_path() {
        let toml = r#"
[[target]]
name = "zaino-state"
path = "/etc/zaino-state"
"#;
        let path = write_temp(toml);
        let err = load(&path).expect_err("absolute path should fail");
        std::fs::remove_file(&path).ok();
        assert!(matches!(err, ConfigError::InvalidPath { .. }));
    }

    #[test]
    fn rejects_traversal_target_path() {
        let toml = r#"
[[target]]
name = "zaino-state"
path = "../escape"
"#;
        let path = write_temp(toml);
        let err = load(&path).expect_err("traversal path should fail");
        std::fs::remove_file(&path).ok();
        assert!(matches!(err, ConfigError::InvalidPath { .. }));
    }

    #[test]
    fn rejects_missing_file() {
        let missing = std::env::temp_dir().join("relman-config-does-not-exist.toml");
        let err = load(&missing).expect_err("missing file should fail");
        assert!(matches!(err, ConfigError::Io { .. }));
    }

    /// The real repo-committed manifest parses and yields the governed set.
    ///
    /// Path is resolved from this crate's manifest dir up to the repo root,
    /// so the test is independent of the process working directory.
    #[test]
    fn loads_real_repo_manifest() {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../../relman.toml");
        let config = load(&manifest).expect("repo relman.toml should parse");

        // The 13 crates.io-published targets from the release ADR § Context.
        assert_eq!(config.targets().len(), 13);
        for name in [
            "zainod",
            "zaino-serve",
            "zaino-state",
            "zaino-proto",
            "zaino-common",
            "zaino-primitives",
            "zaino-address",
            "zaino-source",
            "zaino-rpc",
            "zaino-convert-zebra",
            "zaino-source-zebra-rpc",
            "zaino-source-zebra-readstate",
            "zaino-source-zebra",
        ] {
            let crate_name = CrateName::parse(name).expect("governed name is valid");
            assert!(
                config.target_by_name(&crate_name).is_some(),
                "missing governed target {name}"
            );
        }

        // A source file maps to its owning target via path prefix.
        let owner = config
            .target_owning_path(Path::new("packages/zaino-state/src/lib.rs"))
            .expect("should map to zaino-state");
        assert_eq!(owner.name().as_str(), "zaino-state");
    }

    #[test]
    fn target_owning_path_picks_longest_prefix() {
        let toml = r#"
[[target]]
name = "zaino-state"
path = "packages/zaino-state"

[[target]]
name = "zaino-state-inner"
path = "packages/zaino-state/inner"

[[target]]
name = "zainod"
path = "packages/zainod"
"#;
        let path = write_temp(toml);
        let config = load(&path).expect("valid config");
        std::fs::remove_file(&path).ok();

        let file = Path::new("packages/zaino-state/src/lib.rs");
        let owner = config
            .target_owning_path(file)
            .expect("should map to a target");
        assert_eq!(owner.name().as_str(), "zaino-state");

        // A file under the nested target resolves to the longer prefix.
        let nested = Path::new("packages/zaino-state/inner/src/lib.rs");
        let owner = config
            .target_owning_path(nested)
            .expect("should map to nested");
        assert_eq!(owner.name().as_str(), "zaino-state-inner");

        // A file outside every target has no owner.
        assert!(
            config
                .target_owning_path(Path::new("live-tests/e2e/x.rs"))
                .is_none()
        );
    }
}
