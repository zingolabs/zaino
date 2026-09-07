use std::path::PathBuf;

use crate::ports::{Vcs, VcsError};

/// A [`Vcs`] that returns a preset list of changed paths, ignoring `base`.
///
/// Deterministic stand-in for the git adapter: seed it with exactly the
/// repo-relative paths a test wants a PR's diff to contain.
pub struct StubVcs {
    changed: Vec<PathBuf>,
}

impl StubVcs {
    /// Build a stub over the paths it should report as changed.
    pub fn new(changed: Vec<PathBuf>) -> Self {
        Self { changed }
    }
}

impl Vcs for StubVcs {
    fn changed_files(&self, _base: &str) -> Result<Vec<PathBuf>, VcsError> {
        Ok(self.changed.clone())
    }
}
