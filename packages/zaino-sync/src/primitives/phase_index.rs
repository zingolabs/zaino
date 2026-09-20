//! Phase index newtype.

/// A topological phase in the dependency DAG.
///
/// Phase 0 contains indexes with no dependencies. Each subsequent phase
/// contains indexes whose dependencies are all in earlier phases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PhaseIndex(u32);

impl PhaseIndex {
    /// Create a phase index.
    pub const fn new(index: u32) -> Self {
        Self(index)
    }

    /// The raw numeric value.
    pub const fn value(&self) -> u32 {
        self.0
    }
}

impl core::fmt::Display for PhaseIndex {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0)
    }
}
