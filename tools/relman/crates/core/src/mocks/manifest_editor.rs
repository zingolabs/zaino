use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::ports::{ManifestEditor, ManifestError};
use crate::types::{CrateName, Version};

/// An in-memory [`ManifestEditor`] that records every call instead of touching
/// the filesystem.
///
/// Makes `BumpService` tests deterministic and I/O-free: the service's edits
/// land in interior-mutable vecs the test then asserts against. Interior
/// mutability via `Mutex` keeps the port's `&self` signature (the real
/// `toml_edit` adapter is likewise shared).
#[derive(Default)]
pub struct RecordingManifestEditor {
    package_versions: Mutex<Vec<(PathBuf, Version)>>,
    workspace_deps: Mutex<Vec<(PathBuf, CrateName, Version)>>,
}

impl RecordingManifestEditor {
    /// A fresh recorder with no calls yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// The `set_package_version` calls, in call order.
    pub fn package_version_calls(&self) -> Vec<(PathBuf, Version)> {
        self.package_versions
            .lock()
            .expect("RecordingManifestEditor mutex poisoned")
            .clone()
    }

    /// The `set_workspace_dep_version` calls, in call order.
    pub fn workspace_dep_calls(&self) -> Vec<(PathBuf, CrateName, Version)> {
        self.workspace_deps
            .lock()
            .expect("RecordingManifestEditor mutex poisoned")
            .clone()
    }
}

impl ManifestEditor for RecordingManifestEditor {
    fn set_package_version(
        &self,
        manifest_path: &Path,
        version: &Version,
    ) -> Result<(), ManifestError> {
        self.package_versions
            .lock()
            .expect("RecordingManifestEditor mutex poisoned")
            .push((manifest_path.to_path_buf(), version.clone()));
        Ok(())
    }

    fn set_workspace_dep_version(
        &self,
        root_manifest: &Path,
        dep: &CrateName,
        version: &Version,
    ) -> Result<bool, ManifestError> {
        self.workspace_deps
            .lock()
            .expect("RecordingManifestEditor mutex poisoned")
            .push((root_manifest.to_path_buf(), dep.clone(), version.clone()));
        // The recorder treats every crate as pinned; `BumpService` ignores the
        // boolean, and the string/inline/absent pin distinction is exercised by
        // the real adapter's tests, not here.
        Ok(true)
    }
}
