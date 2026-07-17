//! Capability: the tip a snapshot is pinned to.

use crate::block_id::BlockId;

/// The tip a snapshot is pinned to.
///
/// Synchronous and infallible by design: pinning captures the tip at
/// snapshot creation, so this is a property of the snapshot, not a
/// query against the engine.
pub trait GetPinnedTip: Send + Sync {
    /// The best block as of the moment the snapshot was taken.
    fn get_pinned_tip(&self) -> BlockId;
}
