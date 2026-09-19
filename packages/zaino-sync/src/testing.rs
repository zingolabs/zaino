//! Test utilities: mock provisioner, demo index sets, and backend re-exports.
//!
//! `InMemoryBackend` and `SlowBackend` are re-exported from
//! [`zaino_persistence::in_memory`] (enabled by the `testing` feature).
//! Sync-specific test utilities (`MockProvisioner`, `TestBlockContext`)
//! are defined here.

#[cfg(test)]
mod bench;
// Deferred to the convergence's Phase 2 (provisioner over dev's `zaino-source`):
// this module wires the #1402 `MockChain` source API (`get_block`) and builds
// contexts from the older primitive shapes. It is rewired when the concrete
// source-backed provisioner is productionised over dev's `zaino-source`.
// #[cfg(test)]
// mod source_integration;

/// Toy indexes over [`TestBlockContext`], for exercising the engine end to end
/// (used by this crate's tests and by downstream driver crates behind the
/// `testing` feature).
#[cfg(any(test, feature = "testing"))]
pub mod toy_indexes;

// Re-export persistence testing backends.
#[cfg(any(test, feature = "testing"))]
pub use zaino_persistence::in_memory::{InMemoryBackend, SlowBackend};

/// A toy index set (three BlockLocal indexes) over [`TestBlockContext`], for
/// driving the engine in tests without a real chain source.
#[cfg(any(test, feature = "testing"))]
pub fn toy_index_set() -> crate::index_set::IndexSet<TestBlockContext> {
    use toy_indexes::{
        count_index::CountIndex, running_sum_index::RunningSumIndex, value_index::ValueIndex,
    };
    crate::index_set::IndexSet::new()
        .with::<ValueIndex>()
        .with::<CountIndex>()
        .with::<RunningSumIndex>()
}

use crate::primitives::BlockHeight;
use crate::provisioner::{ProvisionError, Provisioner};

/// Set-wide block context for tests.
///
/// The provisioner produces one of these per block. Individual indexes
/// declare narrower [`BlockContext`](crate::traits::IndexDef::BlockContext)
/// types and receive projections via [`ProvideContext`](crate::traits::ProvideContext).
#[derive(Debug, Clone)]
pub struct TestBlockContext {
    /// Block height.
    pub height: u64,
    /// Arbitrary value carried by this block.
    pub value: u32,
}

/// Mock provisioner that generates `TestBlockContext`s with predictable values.
pub struct MockProvisioner {
    /// Function that produces the value for a given height.
    value_fn: Box<dyn Fn(u64) -> u32 + Send + Sync>,
}

impl MockProvisioner {
    /// Create a provisioner where each block's value equals its height.
    pub fn identity() -> Self {
        Self {
            value_fn: Box::new(|h| h as u32),
        }
    }
}

impl Provisioner for MockProvisioner {
    type BlockContext = TestBlockContext;

    fn provision_range(
        &self,
        from: BlockHeight,
        to: BlockHeight,
    ) -> Result<Vec<Self::BlockContext>, ProvisionError> {
        let blocks = (from.value()..=to.value())
            .map(|h| TestBlockContext {
                height: h,
                value: (self.value_fn)(h),
            })
            .collect();
        Ok(blocks)
    }
}
