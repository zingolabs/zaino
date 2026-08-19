use crate::types::WorkspacePath;

/// Workspace-wide release options (the `[options]` table of `relman.toml`).
///
/// Every field is a resolved [`WorkspacePath`]; defaults are applied at the
/// config boundary, so by the time an `ReleaseOptions` exists each location is
/// concrete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseOptions {
    changesets_dir: WorkspacePath,
    root_manifest: WorkspacePath,
    workspace_changelog: WorkspacePath,
}

impl ReleaseOptions {
    /// Construct options from already-validated paths.
    pub fn new(
        changesets_dir: WorkspacePath,
        root_manifest: WorkspacePath,
        workspace_changelog: WorkspacePath,
    ) -> Self {
        Self {
            changesets_dir,
            root_manifest,
            workspace_changelog,
        }
    }

    /// Directory holding the pending `.changesets/` files.
    pub fn changesets_dir(&self) -> &WorkspacePath {
        &self.changesets_dir
    }

    /// The root manifest carrying `[workspace.dependencies]` pins.
    pub fn root_manifest(&self) -> &WorkspacePath {
        &self.root_manifest
    }

    /// The workspace-level changelog.
    pub fn workspace_changelog(&self) -> &WorkspacePath {
        &self.workspace_changelog
    }
}
