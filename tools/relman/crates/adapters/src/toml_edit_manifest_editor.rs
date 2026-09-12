use std::path::Path;

use relman_core::ports::{ManifestEditor, ManifestError};
use relman_core::types::{CrateName, Version};
use toml_edit::{DocumentMut, Formatted, Item, Value};

/// A [`ManifestEditor`] backed by `toml_edit`.
///
/// Parses a manifest into a [`DocumentMut`], mutates exactly the one version
/// field, and writes the document back — so every comment, blank line, and bit
/// of spacing outside the edited value survives untouched.
#[derive(Default)]
pub struct TomlEditManifestEditor;

impl TomlEditManifestEditor {
    /// A ready-to-use editor.
    pub fn new() -> Self {
        Self
    }

    fn read_doc(path: &Path) -> Result<DocumentMut, ManifestError> {
        let text = std::fs::read_to_string(path).map_err(|source| ManifestError::Io {
            path: path.display().to_string(),
            source,
        })?;
        text.parse::<DocumentMut>()
            .map_err(|error| ManifestError::TomlParse {
                path: path.display().to_string(),
                message: error.to_string(),
            })
    }

    fn write_doc(path: &Path, doc: &DocumentMut) -> Result<(), ManifestError> {
        std::fs::write(path, doc.to_string()).map_err(|source| ManifestError::Io {
            path: path.display().to_string(),
            source,
        })
    }
}

/// Replace a string value's text while preserving its surrounding decor (the
/// whitespace/comments around the `=` and after the value), so only the quoted
/// version characters change.
fn replace_string(slot: &mut Formatted<String>, version: &Version) {
    let decor = slot.decor().clone();
    *slot = Formatted::new(version.to_string());
    *slot.decor_mut() = decor;
}

/// Set the version carried by `item`, if it carries one, returning whether it
/// did. Handles the string form (`"1.2.3"`) directly and the table form
/// (inline or otherwise) by updating its `version` key; a table with no
/// `version` key (a path-only pin) is left untouched.
fn update_version_in_item(item: &mut Item, version: &Version) -> bool {
    if let Item::Value(Value::String(slot)) = item {
        replace_string(slot, version);
        return true;
    }
    if let Some(table) = item.as_table_like_mut()
        && let Some(Item::Value(Value::String(slot))) = table.get_mut("version")
    {
        replace_string(slot, version);
        return true;
    }
    false
}

/// Whether `item` is the inherited form `version.workspace = true` (written
/// either inline or as a dotted key), which relman refuses to edit.
fn is_workspace_inherited(item: &Item) -> bool {
    item.as_table_like()
        .and_then(|table| table.get("workspace"))
        .and_then(Item::as_bool)
        == Some(true)
}

impl ManifestEditor for TomlEditManifestEditor {
    fn set_package_version(
        &self,
        manifest_path: &Path,
        version: &Version,
    ) -> Result<(), ManifestError> {
        let mut doc = Self::read_doc(manifest_path)?;
        let package = doc
            .get_mut("package")
            .and_then(Item::as_table_like_mut)
            .ok_or_else(|| ManifestError::MissingPackageTable {
                path: manifest_path.display().to_string(),
            })?;

        if let Some(existing) = package.get("version")
            && is_workspace_inherited(existing)
        {
            return Err(ManifestError::VersionIsWorkspaceInherited {
                path: manifest_path.display().to_string(),
            });
        }

        let updated = match package.get_mut("version") {
            Some(item) => update_version_in_item(item, version),
            None => false,
        };
        if !updated {
            // No literal-string version (or none at all): write a fresh pin,
            // preserving every other `[package]` key and its formatting.
            package.insert("version", toml_edit::value(version.to_string()));
        }

        Self::write_doc(manifest_path, &doc)
    }

    fn set_workspace_dep_version(
        &self,
        root_manifest: &Path,
        dep: &CrateName,
        version: &Version,
    ) -> Result<bool, ManifestError> {
        let mut doc = Self::read_doc(root_manifest)?;

        let Some(deps) = doc
            .get_mut("workspace")
            .and_then(Item::as_table_like_mut)
            .and_then(|workspace| workspace.get_mut("dependencies"))
            .and_then(Item::as_table_like_mut)
        else {
            // No `[workspace.dependencies]` table at all.
            return Ok(false);
        };

        let Some(item) = deps.get_mut(dep.as_str()) else {
            // This crate is not pinned centrally.
            return Ok(false);
        };

        if update_version_in_item(item, version) {
            Self::write_doc(root_manifest, &doc)?;
            Ok(true)
        } else {
            // Present but path-only (no `version` key): nothing to update.
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    fn version(raw: &str) -> Version {
        Version::parse(raw).expect("valid version")
    }

    fn crate_name(raw: &str) -> CrateName {
        CrateName::parse(raw).expect("valid crate name")
    }

    /// Write `contents` to a fresh temp file and return its path plus the
    /// guard that keeps the directory alive for the test's duration.
    fn temp_manifest(contents: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("Cargo.toml");
        std::fs::write(&path, contents).expect("seed manifest");
        (dir, path)
    }

    #[test]
    fn set_package_version_changes_only_the_version_and_preserves_the_rest() {
        let original = "\
[package]
name = \"zaino-state\"       # keep this comment
# a standalone comment
version = \"0.6.0\"
edition = \"2024\"

[dependencies]
serde = \"1\"
";
        let (_dir, path) = temp_manifest(original);
        let editor = TomlEditManifestEditor::new();
        editor
            .set_package_version(&path, &version("0.6.1"))
            .expect("sets version");

        let updated = std::fs::read_to_string(&path).expect("read back");
        let expected = original.replace("version = \"0.6.0\"", "version = \"0.6.1\"");
        assert_eq!(updated, expected, "only the version literal should change");
    }

    #[test]
    fn set_package_version_rejects_workspace_inherited_version() {
        for inherited in [
            "[package]\nname = \"x\"\nversion.workspace = true\n",
            "[package]\nname = \"x\"\nversion = { workspace = true }\n",
        ] {
            let (_dir, path) = temp_manifest(inherited);
            let editor = TomlEditManifestEditor::new();
            let err = editor
                .set_package_version(&path, &version("1.0.0"))
                .expect_err("inherited version should be refused");
            assert!(matches!(
                err,
                ManifestError::VersionIsWorkspaceInherited { .. }
            ));
            // The file must be left untouched on refusal.
            assert_eq!(
                std::fs::read_to_string(&path).expect("read back"),
                inherited
            );
        }
    }

    #[test]
    fn set_package_version_errors_without_a_package_table() {
        let (_dir, path) = temp_manifest("[workspace]\nmembers = []\n");
        let editor = TomlEditManifestEditor::new();
        let err = editor
            .set_package_version(&path, &version("1.0.0"))
            .expect_err("missing [package] should error");
        assert!(matches!(err, ManifestError::MissingPackageTable { .. }));
    }

    #[test]
    fn set_workspace_dep_version_updates_a_string_pin() {
        let original = "\
[workspace.dependencies]
# central pins
zaino-proto = \"0.3.0\"
zaino-state = \"0.6.0\"
";
        let (_dir, path) = temp_manifest(original);
        let editor = TomlEditManifestEditor::new();
        let updated = editor
            .set_workspace_dep_version(&path, &crate_name("zaino-state"), &version("0.7.0"))
            .expect("updates pin");
        assert!(updated, "an existing string pin should report updated");

        let text = std::fs::read_to_string(&path).expect("read back");
        let expected = original.replace("zaino-state = \"0.6.0\"", "zaino-state = \"0.7.0\"");
        assert_eq!(text, expected, "only the one pin's version should change");
    }

    #[test]
    fn set_workspace_dep_version_updates_an_inline_table_pin_and_keeps_other_keys() {
        let original = "\
[workspace.dependencies]
zaino-state = { path = \"packages/zaino-state\", version = \"0.6.0\", default-features = false }
zaino-serve = { path = \"packages/zaino-serve\", version = \"0.5.1\", default-features = false }
";
        let (_dir, path) = temp_manifest(original);
        let editor = TomlEditManifestEditor::new();
        let updated = editor
            .set_workspace_dep_version(&path, &crate_name("zaino-state"), &version("0.7.0"))
            .expect("updates pin");
        assert!(
            updated,
            "an existing inline-table pin should report updated"
        );

        let text = std::fs::read_to_string(&path).expect("read back");
        let expected = original.replace(
            "version = \"0.6.0\", default-features = false }",
            "version = \"0.7.0\", default-features = false }",
        );
        assert_eq!(
            text, expected,
            "path/default-features and formatting must be preserved"
        );
    }

    #[test]
    fn set_workspace_dep_version_returns_false_for_an_absent_dep() {
        let original = "[workspace.dependencies]\nzaino-proto = \"0.3.0\"\n";
        let (_dir, path) = temp_manifest(original);
        let editor = TomlEditManifestEditor::new();
        let updated = editor
            .set_workspace_dep_version(&path, &crate_name("zaino-state"), &version("0.7.0"))
            .expect("absent dep is not an error");
        assert!(!updated, "an absent dep should report not-updated");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read back"),
            original,
            "an absent dep must leave the manifest untouched"
        );
    }

    #[test]
    fn set_workspace_dep_version_returns_false_for_a_path_only_pin() {
        let original = "[workspace.dependencies]\nzainod = { path = \"packages/zainod\" }\n";
        let (_dir, path) = temp_manifest(original);
        let editor = TomlEditManifestEditor::new();
        let updated = editor
            .set_workspace_dep_version(&path, &crate_name("zainod"), &version("0.7.0"))
            .expect("path-only pin is not an error");
        assert!(!updated, "a path-only pin has no version to update");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read back"),
            original,
            "a path-only pin must be left untouched"
        );
    }
}
