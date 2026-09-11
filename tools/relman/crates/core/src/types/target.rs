use crate::types::{CrateName, WorkspacePath};

/// A single governed versioning target (one `[[target]]` in `relman.toml`).
///
/// A target ties a governed [`CrateName`] to its directory in the working
/// tree, its (resolved, non-optional) changelog location, and whether it is
/// `cargo publish`ed. Defaults (`changelog`, `publish`) are applied at the
/// config boundary, so every field here is concrete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    name: CrateName,
    path: WorkspacePath,
    changelog: WorkspacePath,
    publish: bool,
}

impl Target {
    /// Construct a target from already-validated parts.
    pub fn new(
        name: CrateName,
        path: WorkspacePath,
        changelog: WorkspacePath,
        publish: bool,
    ) -> Self {
        Self {
            name,
            path,
            changelog,
            publish,
        }
    }

    /// The governed crate name.
    pub fn name(&self) -> &CrateName {
        &self.name
    }

    /// The target's directory, relative to the repo root.
    pub fn path(&self) -> &WorkspacePath {
        &self.path
    }

    /// The target's changelog file.
    pub fn changelog(&self) -> &WorkspacePath {
        &self.changelog
    }

    /// Whether the target is published to crates.io.
    pub fn publish(&self) -> bool {
        self.publish
    }
}
