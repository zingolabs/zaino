use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use cargo_metadata::{DependencyKind, MetadataCommand};
use relman_core::ports::{Workspace, WorkspaceError};
use relman_core::types::{CrateName, Version};

/// A [`Workspace`] backed by `cargo metadata` over the repo-root manifest.
///
/// Delegating to `cargo metadata` (rather than hand-parsing `Cargo.toml`)
/// resolves `version.workspace` and `workspace.dependencies` inheritance for
/// free, so a crate declaring `version.workspace = true` or
/// `dep = { workspace = true }` still reports its concrete version and
/// requirement. Everything is filtered to the **governed set** — the targets
/// declared in `relman.toml` — so external crates and internal-only members
/// never appear.
pub struct CargoMetadataWorkspace {
    manifest_path: PathBuf,
    governed: BTreeSet<CrateName>,
}

impl CargoMetadataWorkspace {
    /// Root the adapter at `manifest_path` (the repo-root `Cargo.toml`),
    /// restricting results to `governed`.
    pub fn new(manifest_path: PathBuf, governed: BTreeSet<CrateName>) -> Self {
        Self {
            manifest_path,
            governed,
        }
    }

    /// Run `cargo metadata` and return the resolved workspace model.
    fn metadata(&self) -> Result<cargo_metadata::Metadata, WorkspaceError> {
        MetadataCommand::new()
            .manifest_path(&self.manifest_path)
            .exec()
            .map_err(|err| WorkspaceError::Backend {
                message: err.to_string(),
            })
    }

    /// Whether `name` (a raw cargo package name) is a governed target.
    fn governed_name(&self, name: &str) -> Option<CrateName> {
        // Governed names are parsed CrateNames; a raw package name matches iff it
        // parses and is in the set. Parsing cannot widen the set, so this is a
        // pure membership test.
        let parsed = CrateName::parse(name).ok()?;
        self.governed.contains(&parsed).then_some(parsed)
    }
}

impl Workspace for CargoMetadataWorkspace {
    fn versions(&self) -> Result<BTreeMap<CrateName, Version>, WorkspaceError> {
        let metadata = self.metadata()?;
        let members: BTreeSet<_> = metadata.workspace_members.iter().collect();

        let mut versions = BTreeMap::new();
        for package in &metadata.packages {
            if !members.contains(&package.id) {
                continue;
            }
            if let Some(name) = self.governed_name(&package.name) {
                versions.insert(name, Version::from_semver(package.version.clone()));
            }
        }

        // Every governed target must be present, or the two manifests drifted.
        for target in &self.governed {
            if !versions.contains_key(target) {
                return Err(WorkspaceError::MissingTarget {
                    crate_name: target.as_str().to_owned(),
                });
            }
        }
        Ok(versions)
    }

    fn internal_deps(
        &self,
    ) -> Result<BTreeMap<CrateName, Vec<(CrateName, semver::VersionReq)>>, WorkspaceError> {
        let metadata = self.metadata()?;
        let members: BTreeSet<_> = metadata.workspace_members.iter().collect();

        let mut edges = BTreeMap::new();
        for package in &metadata.packages {
            if !members.contains(&package.id) {
                continue;
            }
            let Some(dependent) = self.governed_name(&package.name) else {
                continue;
            };

            // Dedup edges to the same governed dependency (a dep can appear under
            // several kinds/targets); keep the first declared requirement. Dev
            // dependencies are excluded: they are not part of the crate's
            // published dependency contract.
            let mut per_dep: BTreeMap<CrateName, semver::VersionReq> = BTreeMap::new();
            for dep in &package.dependencies {
                if dep.kind == DependencyKind::Development {
                    continue;
                }
                if let Some(dependency) = self.governed_name(&dep.name) {
                    if dependency == dependent {
                        continue; // Never record a self-edge.
                    }
                    per_dep.entry(dependency).or_insert_with(|| dep.req.clone());
                }
            }
            if !per_dep.is_empty() {
                edges.insert(dependent, per_dep.into_iter().collect());
            }
        }
        Ok(edges)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn name(raw: &str) -> CrateName {
        CrateName::parse(raw).expect("valid crate name")
    }

    /// Write a member crate `<root>/<dir>/{Cargo.toml,src/lib.rs}`.
    fn write_member(root: &Path, dir: &str, manifest: &str) {
        let crate_dir = root.join(dir);
        fs::create_dir_all(crate_dir.join("src")).expect("mkdir");
        fs::write(crate_dir.join("Cargo.toml"), manifest).expect("write manifest");
        fs::write(crate_dir.join("src/lib.rs"), "").expect("write lib");
    }

    /// A tiny path-only workspace (no crates.io deps → `cargo metadata` runs
    /// fully offline): `dependent` 0.5.0 depends on `dependency` 0.3.1 with the
    /// given `req`, plus an ungoverned `internal` crate that must be filtered out.
    fn build_workspace(root: &Path, req: &str) {
        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nresolver = \"2\"\n\
             members = [\"dependency\", \"dependent\", \"internal\"]\n",
        )
        .expect("write root");

        write_member(
            root,
            "dependency",
            "[package]\nname = \"dependency\"\nversion = \"0.3.1\"\nedition = \"2021\"\n",
        );
        write_member(
            root,
            "dependent",
            &format!(
                "[package]\nname = \"dependent\"\nversion = \"0.5.0\"\nedition = \"2021\"\n\
                 [dependencies]\ndependency = {{ path = \"../dependency\", version = \"{req}\" }}\n"
            ),
        );
        // Not in the governed set; its edge to `dependency` must not appear.
        write_member(
            root,
            "internal",
            "[package]\nname = \"internal\"\nversion = \"0.0.1\"\nedition = \"2021\"\n\
             [dependencies]\ndependency = { path = \"../dependency\", version = \"0.3\" }\n",
        );
    }

    fn adapter(root: &Path) -> CargoMetadataWorkspace {
        CargoMetadataWorkspace::new(
            root.join("Cargo.toml"),
            [name("dependency"), name("dependent")]
                .into_iter()
                .collect(),
        )
    }

    #[test]
    fn reads_governed_versions_only() {
        let tmp = tempfile::tempdir().expect("temp dir");
        build_workspace(tmp.path(), "0.3");
        let versions = adapter(tmp.path()).versions().expect("versions");

        assert_eq!(versions.len(), 2, "ungoverned `internal` must be excluded");
        assert_eq!(
            versions.get(&name("dependency")).map(|v| v.to_string()),
            Some("0.3.1".to_owned())
        );
        assert_eq!(
            versions.get(&name("dependent")).map(|v| v.to_string()),
            Some("0.5.0".to_owned())
        );
    }

    #[test]
    fn reads_internal_dep_edges_with_declared_req() {
        let tmp = tempfile::tempdir().expect("temp dir");
        build_workspace(tmp.path(), "0.3");
        let deps = adapter(tmp.path()).internal_deps().expect("edges");

        // Only the governed dependent has an edge; `internal` is filtered out.
        let dependent_edges = deps.get(&name("dependent")).expect("dependent has edges");
        assert_eq!(dependent_edges.len(), 1);
        let (dep, req) = &dependent_edges[0];
        assert_eq!(dep, &name("dependency"));
        // A `version = "0.3"` requirement resolves to the caret req `^0.3`.
        assert!(req.matches(&semver::Version::new(0, 3, 9)));
        assert!(!req.matches(&semver::Version::new(0, 4, 0)));

        assert!(
            !deps.contains_key(&name("internal")),
            "ungoverned crate must not appear as a dependent"
        );
    }

    #[test]
    fn missing_governed_target_errors() {
        let tmp = tempfile::tempdir().expect("temp dir");
        build_workspace(tmp.path(), "0.3");
        // Govern a crate the workspace does not contain.
        let adapter = CargoMetadataWorkspace::new(
            tmp.path().join("Cargo.toml"),
            [name("dependency"), name("nonexistent")]
                .into_iter()
                .collect(),
        );
        let err = adapter.versions().expect_err("missing target should error");
        assert!(matches!(
            err,
            WorkspaceError::MissingTarget { crate_name } if crate_name == "nonexistent"
        ));
    }
}
