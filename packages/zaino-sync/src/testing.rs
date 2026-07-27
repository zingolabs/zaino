//! Test utilities: mock provisioner, demo index sets, and backend re-exports.
//!
//! `InMemoryBackend` and `SlowBackend` are re-exported from
//! [`zaino_persistence::in_memory`] (enabled by the `testing` feature).
//! Sync-specific test utilities (`MockProvisioner`, `TestBlockContext`)
//! are defined here.

#[cfg(test)]
mod bench;
#[cfg(test)]
mod source_integration;
#[cfg(test)]
mod toy_indexes;

// Re-export persistence testing backends.
#[cfg(any(test, feature = "testing"))]
pub use zaino_persistence::in_memory::{InMemoryBackend, SlowBackend};

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
