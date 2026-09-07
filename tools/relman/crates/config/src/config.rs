use std::path::Path;

use relman_core::types::{CrateName, ReleaseOptions, Target};

/// The parsed `relman.toml` — the authority for relman's governed targets.
///
/// Built by [`load`](crate::load) from the repo-committed manifest: raw serde
/// structs are parsed into core newtypes at the boundary and defaults applied,
/// so a `ReleaseConfig` is always internally valid (≥1 target, unique names,
/// resolved per-target changelog/publish).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseConfig {
    options: ReleaseOptions,
    targets: Vec<Target>,
}

impl ReleaseConfig {
    /// Construct from already-validated parts. Kept `pub(crate)` so the only
    /// public path to a `ReleaseConfig` is through [`load`](crate::load).
    pub(crate) fn new(options: ReleaseOptions, targets: Vec<Target>) -> Self {
        Self { options, targets }
    }

    /// Build a config from already-validated parts, for downstream unit tests
    /// that need a hand-built target set without a `relman.toml` on disk.
    #[cfg(feature = "test-support")]
    pub fn for_test(options: ReleaseOptions, targets: Vec<Target>) -> Self {
        Self::new(options, targets)
    }

    /// The workspace-wide `[options]`.
    pub fn options(&self) -> &ReleaseOptions {
        &self.options
    }

    /// The governed versioning targets.
    pub fn targets(&self) -> &[Target] {
        &self.targets
    }

    /// Look up a target by its exact crate name.
    pub fn target_by_name(&self, name: &CrateName) -> Option<&Target> {
        self.targets.iter().find(|target| target.name() == name)
    }

    /// Find the target that owns `file`, i.e. the target whose `path` is the
    /// longest component-wise prefix of `file`.
    ///
    /// This drives changed-file → target mapping for changeset enforcement.
    /// When two target paths both prefix `file` (a nested layout), the longer
    /// — more specific — one wins.
    pub fn target_owning_path(&self, file: &Path) -> Option<&Target> {
        self.targets
            .iter()
            .filter(|target| file.starts_with(target.path().as_path()))
            .max_by_key(|target| target.path().as_path().components().count())
    }
}
