//! Integrity and size reports a [`Backend`](crate::Backend) answers.

use crate::backend::Namespace;

/// What [`Backend::verify`](crate::Backend::verify) found: one unit per stored file
/// (or the backend's equivalent); none when there is nothing to check.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Verification {
    /// Each unit checked.
    pub units: Vec<Unit>,
}

impl Verification {
    /// No unit damaged ([`Problem::Orphaned`] is harmless).
    pub fn is_clean(&self) -> bool {
        self.units
            .iter()
            .flat_map(|unit| &unit.problems)
            .all(|problem| !problem.is_damage())
    }
}

/// One stored unit and what its check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unit {
    /// The unit's name (e.g. a file path under the backend's directory).
    pub name: String,
    /// What is wrong with it; empty = clean.
    pub problems: Vec<Problem>,
}

/// A finding against a stored unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// The unit is gone.
    Missing,
    /// Shorter than committed.
    Truncated,
    /// A page's bytes differ from its checksum.
    ChecksumMismatch {
        /// Page index.
        page: u64,
    },
    /// The unit's checksum list differs from what the commit recorded.
    ChecksumListMismatch,
    /// Bytes past what the commit claims (an interrupted commit's tail: harmless).
    Orphaned {
        /// Bytes past the committed length.
        bytes: u64,
    },
}

impl Problem {
    /// Whether committed data is affected.
    pub fn is_damage(&self) -> bool {
        !matches!(self, Self::Orphaned { .. })
    }
}

/// What one namespace occupies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NamespaceFootprint {
    /// The namespace.
    pub namespace: Namespace,
    /// Bytes stored.
    pub bytes: u64,
    /// Of `bytes`, those in the page cache; `None` = not knowable.
    pub cached: Option<u64>,
}
