//! Capability: subscribe to chain tip changes.

use futures_core::Stream;

use crate::block_id::BlockId;

/// Subscribe to chain tip changes.
///
/// This is the explicit new-block signal of ADR 0001: drivers learn of
/// chain movement here, and the mempool stream no longer carries that
/// news by ending.
///
/// Semantics: the first event is the tip current at subscription time
/// (an engine with no view yet delivers its first tip when one
/// exists); every event carries the new tip; events may coalesce under
/// load, but the latest tip is always eventually delivered; a reorg
/// emits an event like any other movement, and the new tip may sit at
/// or below the old height. The stream ends only when the port shuts
/// down.
pub trait SubscribeToTipChanges: Send + Sync {
    /// Tip events, starting from the current tip.
    fn subscribe_to_tip_changes(&self) -> impl Stream<Item = BlockId> + Send;
}
