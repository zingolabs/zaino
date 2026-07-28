//! Subscription: wake on best-chain tip changes.

use tokio::sync::watch;
use zaino_primitives::types::{BlockHash, Height};

/// Subscribe to best-chain tip changes.
///
/// Unlike [`SubscribeBlocks`](super::SubscribeBlocks), which only signals
/// *that* the source advanced, this carries the tip itself — so a consumer
/// that just wants the current tip does not have to issue a follow-up query,
/// and cannot observe a tip newer than the one that woke it.
///
/// The two are separate capabilities because they answer different questions.
/// A block arriving is not necessarily a tip change: a block extending a side
/// chain wakes [`SubscribeBlocks`](super::SubscribeBlocks) but leaves the best
/// tip where it was.
///
/// [`watch`] coalesces, so a consumer that falls behind a burst sees the latest
/// tip rather than a backlog — which is what a tip subscriber wants, since
/// intermediate tips are already stale by the time they are read.
///
/// Returning `None` means the source has no local tip stream to expose. Only an
/// adapter that observes the chain directly can offer one; adapters that reach
/// the validator over a request/response transport learn of tip changes by
/// asking, and inherit this default.
pub trait SubscribeChainTip: Send + Sync {
    /// Subscribe to tip changes, if this source tracks the tip locally.
    ///
    /// The channel carries the same `(hash, height)` pair as
    /// [`GetChainTip`](super::GetChainTip), so the two agree on what a tip is.
    fn subscribe_to_chain_tip(&self) -> Option<watch::Receiver<(BlockHash, Height)>> {
        None
    }
}
