use std::path::PathBuf;
use std::sync::Arc;

use relman_config::ReleaseConfig;
use relman_core::ports::{ApplyBump, ApplyError, ManifestEditor};
use relman_core::types::{BumpTable, CrateBump};

/// The `Cargo.toml` file name, joined onto a target's directory to reach its
/// crate manifest.
const MANIFEST_NAME: &str = "Cargo.toml";

/// Applies a derived [`BumpTable`] to the workspace manifests. Implements the
/// [`ApplyBump`] driving port over the [`ManifestEditor`] driven port and the
/// loaded [`ReleaseConfig`].
///
/// For every bumping crate it does two edits:
///
/// 1. sets that crate's `[package] version` in `<target path>/Cargo.toml`;
/// 2. updates the crate's pin in the root manifest's
///    `[workspace.dependencies]` — how a bumped crate's new version reaches its
///    dependents (a crate not centrally pinned is simply skipped).
///
/// Every referenced crate is resolved against the config *before* any edit, so
/// a bump naming an unknown target fails without leaving a half-edited tree.
pub struct BumpService {
    config: ReleaseConfig,
    editor: Arc<dyn ManifestEditor>,
}

impl BumpService {
    pub fn new(config: ReleaseConfig, editor: Arc<dyn ManifestEditor>) -> Self {
        Self { config, editor }
    }

    /// Resolve `bump`'s crate to its manifest path, erroring if it is not a
    /// declared target.
    fn manifest_path(&self, bump: &CrateBump) -> Result<PathBuf, ApplyError> {
        let name = bump.crate_name();
        let target = self
            .config
            .target_by_name(name)
            .ok_or_else(|| ApplyError::UnknownTarget {
                crate_name: name.as_str().to_owned(),
            })?;
        Ok(target.path().as_path().join(MANIFEST_NAME))
    }
}

impl ApplyBump for BumpService {
    fn apply(&self, table: &BumpTable) -> Result<(), ApplyError> {
        // Resolve every target up front so an unknown crate aborts before we
        // touch any file.
        let resolved: Vec<(PathBuf, &CrateBump)> = table
            .bumps()
            .iter()
            .map(|bump| Ok((self.manifest_path(bump)?, bump)))
            .collect::<Result<_, ApplyError>>()?;

        // Set each crate's own `[package] version`.
        for (manifest, bump) in &resolved {
            self.editor.set_package_version(manifest, bump.next())?;
        }

        // Update each crate's central pin (ignoring the "not pinned" result).
        let root_manifest = self.config.options().root_manifest().as_path();
        for (_, bump) in &resolved {
            self.editor
                .set_workspace_dep_version(root_manifest, bump.crate_name(), bump.next())?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use relman_core::mocks::RecordingManifestEditor;
    use relman_core::types::{Bump, CrateName, ReleaseOptions, Target, Version, WorkspacePath};

    fn name(raw: &str) -> CrateName {
        CrateName::parse(raw).expect("valid crate name")
    }

    fn version(raw: &str) -> Version {
        Version::parse(raw).expect("valid version")
    }

    fn path(raw: &str) -> WorkspacePath {
        WorkspacePath::parse(raw).expect("valid path")
    }

    fn target(crate_name: &str) -> Target {
        Target::new(
            name(crate_name),
            path(&format!("packages/{crate_name}")),
            path(&format!("packages/{crate_name}/CHANGELOG.md")),
            true,
        )
    }

    fn config(names: &[&str]) -> ReleaseConfig {
        let options = ReleaseOptions::new(
            path(".changesets"),
            path("Cargo.toml"),
            path("CHANGELOG.md"),
        );
        ReleaseConfig::for_test(options, names.iter().map(|n| target(n)).collect())
    }

    fn crate_bump(crate_name: &str, current: &str, next: &str, bump: Bump) -> CrateBump {
        CrateBump::new(
            name(crate_name),
            version(current),
            version(next),
            bump,
            Vec::new(),
        )
    }

    #[test]
    fn applies_package_versions_and_root_pins_for_every_bump() {
        let editor = Arc::new(RecordingManifestEditor::new());
        let service = BumpService::new(config(&["zaino-state", "zainod"]), editor.clone());

        let table = BumpTable::new(vec![
            crate_bump("zaino-state", "0.6.0", "0.7.0", Bump::Minor),
            crate_bump("zainod", "0.4.3", "0.4.4", Bump::Patch),
        ]);
        service.apply(&table).expect("apply succeeds");

        // Exactly one package-version edit per crate, at its manifest path.
        assert_eq!(
            editor.package_version_calls(),
            vec![
                (
                    PathBuf::from("packages/zaino-state/Cargo.toml"),
                    version("0.7.0")
                ),
                (
                    PathBuf::from("packages/zainod/Cargo.toml"),
                    version("0.4.4")
                ),
            ]
        );

        // Exactly one root-pin edit per crate, all against the root manifest.
        assert_eq!(
            editor.workspace_dep_calls(),
            vec![
                (
                    PathBuf::from("Cargo.toml"),
                    name("zaino-state"),
                    version("0.7.0")
                ),
                (
                    PathBuf::from("Cargo.toml"),
                    name("zainod"),
                    version("0.4.4")
                ),
            ]
        );
    }

    #[test]
    fn an_empty_table_edits_nothing() {
        let editor = Arc::new(RecordingManifestEditor::new());
        let service = BumpService::new(config(&["zaino-state"]), editor.clone());
        service
            .apply(&BumpTable::default())
            .expect("apply succeeds");
        assert!(editor.package_version_calls().is_empty());
        assert!(editor.workspace_dep_calls().is_empty());
    }

    #[test]
    fn a_bump_naming_an_unknown_target_errors_before_any_edit() {
        let editor = Arc::new(RecordingManifestEditor::new());
        // Config governs only zaino-state; the table also names zaino-proto.
        let service = BumpService::new(config(&["zaino-state"]), editor.clone());
        let table = BumpTable::new(vec![
            crate_bump("zaino-state", "0.6.0", "0.7.0", Bump::Minor),
            crate_bump("zaino-proto", "0.3.0", "0.3.1", Bump::Patch),
        ]);

        let err = service
            .apply(&table)
            .expect_err("unknown target should error");
        assert!(matches!(
            err,
            ApplyError::UnknownTarget { crate_name } if crate_name == "zaino-proto"
        ));
        // Up-front resolution means no file was touched at all.
        assert!(editor.package_version_calls().is_empty());
        assert!(editor.workspace_dep_calls().is_empty());
    }
}
