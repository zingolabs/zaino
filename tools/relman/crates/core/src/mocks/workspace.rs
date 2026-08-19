use std::collections::BTreeMap;

use crate::ports::{Workspace, WorkspaceError};
use crate::types::{CrateName, Version};

/// An in-memory [`Workspace`] built from preset versions and dependency edges.
///
/// Makes version-derivation tests deterministic and cargo-free: seed the
/// current version of each governed crate and the `(dependent, dependency,
/// req)` edges among them, and the service sees exactly that crate graph.
#[derive(Default)]
pub struct MapWorkspace {
    versions: BTreeMap<CrateName, Version>,
    internal_deps: BTreeMap<CrateName, Vec<(CrateName, semver::VersionReq)>>,
}

impl MapWorkspace {
    /// Build from `versions` (current version per governed crate) and `edges`
    /// (`dependent` depends on `dependency` with `req`).
    pub fn new(
        versions: Vec<(CrateName, Version)>,
        edges: Vec<(CrateName, CrateName, semver::VersionReq)>,
    ) -> Self {
        let mut internal_deps: BTreeMap<CrateName, Vec<(CrateName, semver::VersionReq)>> =
            BTreeMap::new();
        for (dependent, dependency, req) in edges {
            internal_deps
                .entry(dependent)
                .or_default()
                .push((dependency, req));
        }
        Self {
            versions: versions.into_iter().collect(),
            internal_deps,
        }
    }
}

impl Workspace for MapWorkspace {
    fn versions(&self) -> Result<BTreeMap<CrateName, Version>, WorkspaceError> {
        Ok(self.versions.clone())
    }

    fn internal_deps(
        &self,
    ) -> Result<BTreeMap<CrateName, Vec<(CrateName, semver::VersionReq)>>, WorkspaceError> {
        Ok(self.internal_deps.clone())
    }
}
