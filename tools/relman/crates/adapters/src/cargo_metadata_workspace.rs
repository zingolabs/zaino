use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Command;

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

    fn refresh_lockfile(&self) -> Result<(), WorkspaceError> {
        // `--workspace` rewrites only the members' own lock entries, leaving
        // every third-party pin as it was; `--offline` keeps the refresh from
        // touching the registry, which the member-only update never needs.
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let output = Command::new(cargo)
            .arg("update")
            .arg("--workspace")
            .arg("--offline")
            .arg("--manifest-path")
            .arg(&self.manifest_path)
            .output()
            .map_err(|err| WorkspaceError::Backend {
                message: format!("failed to run cargo update: {err}"),
            })?;
        if output.status.success() {
            return Ok(());
        }
        Err(WorkspaceError::Backend {
            message: format!(
                "cargo update --workspace failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        })
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

    /// Every workspace member that a governed target depends on through a
    /// `path` + `version` requirement is published alongside it, so it must be
    /// governed too; otherwise `cargo publish` of the dependent resolves the
    /// requirement against crates.io, where the ungoverned crate is absent or
    /// stale, and nothing in relman notices the gap beforehand.
    #[test]
    fn every_published_dependency_of_a_governed_target_is_governed() {
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../..");
        let config =
            relman_config::load(&repo_root.join("relman.toml")).expect("repo relman.toml loads");
        let governed: BTreeSet<CrateName> = config
            .targets()
            .iter()
            .map(|target| target.name().clone())
            .collect();

        let metadata = MetadataCommand::new()
            .manifest_path(repo_root.join("Cargo.toml"))
            .no_deps()
            .exec()
            .expect("cargo metadata --no-deps over the repo root");
        let members: BTreeSet<_> = metadata.workspace_members.iter().collect();
        let member_names: BTreeSet<&str> = metadata
            .packages
            .iter()
            .filter(|package| members.contains(&package.id))
            .map(|package| package.name.as_str())
            .collect();

        let mut ungoverned: BTreeSet<String> = BTreeSet::new();
        for package in &metadata.packages {
            if !members.contains(&package.id) || !governed.contains(&name(&package.name)) {
                continue;
            }
            for dep in &package.dependencies {
                let published_edge = dep.kind != DependencyKind::Development
                    && dep.path.is_some()
                    && dep.req != semver::VersionReq::STAR;
                if published_edge
                    && member_names.contains(dep.name.as_str())
                    && !governed.contains(&name(&dep.name))
                {
                    ungoverned.insert(format!("{} -> {}", package.name, dep.name));
                }
            }
        }
        assert!(
            ungoverned.is_empty(),
            "governed targets depend on workspace crates relman.toml does not govern:\n{}",
            ungoverned.into_iter().collect::<Vec<_>>().join("\n")
        );
    }

    #[test]
    fn refresh_lockfile_records_a_changed_member_version() {
        let tmp = tempfile::tempdir().expect("temp dir");
        build_workspace(tmp.path(), "0.3");
        let adapter = adapter(tmp.path());
        adapter
            .versions()
            .expect("first metadata run writes Cargo.lock");

        write_member(
            tmp.path(),
            "dependency",
            "[package]\nname = \"dependency\"\nversion = \"0.3.2\"\nedition = \"2021\"\n",
        );
        adapter.refresh_lockfile().expect("refresh succeeds");

        let lock = fs::read_to_string(tmp.path().join("Cargo.lock")).expect("read lock");
        assert!(
            lock.contains("version = \"0.3.2\""),
            "lock not refreshed:\n{lock}"
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
