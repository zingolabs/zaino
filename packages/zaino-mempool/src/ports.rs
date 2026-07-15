//! Ports required by the mempool core.
//!
//! These traits are *consumer-defined*: the mempool core declares exactly what
//! it needs from the outside world, and `zaino-state` supplies adapters. See the
//! crate-level docs for the hexagonal rationale.

use zebra_chain::{
    block::{Hash as BlockHash, Height},
    transaction::{Hash as TxHash, SerializedTransaction},
};

use crate::{MempoolError, SendFut};

/// A minimal `(height, hash)` reference to a block.
///
/// The mempool core uses this instead of `zaino-state`'s `BlockIndex` so it does
/// not depend on `zaino-state`. Adapters map their richer tip types onto this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockRef {
    /// The block's height.
    pub height: Height,
    /// The block's hash.
    pub hash: BlockHash,
}

/// A stable identifier for a published non-finalized-state snapshot.
///
/// `generation` increments exactly once per successfully published
/// non-finalized snapshot, so two epochs with the same `best_tip` but different
/// contents are still distinguishable. Hash-only matching is weaker; the mempool
/// keys coherence on the whole epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NonFinalizedEpoch {
    /// Monotonic publication generation of the non-finalized snapshot.
    pub generation: u64,
    /// The best (tip) block of the non-finalized snapshot.
    pub best_tip: BlockRef,
}

/// Port: the source of mempool data (txids, raw transactions, and the tip of the
/// source that supplies them).
///
/// For the State backend this must be backed by the *same* fetcher that serves
/// mempool txids (Zebra's JSON-RPC `mempool_fetcher`), **not** `ReadStateService`,
/// so the observed tip and the mempool data come from one consistent source.
///
/// Implementations must be cheap to `clone` — the mempool service clones the
/// source to fan out bounded, concurrent raw-transaction fetches.
pub trait MempoolSource: Clone + Send + Sync + 'static {
    /// Returns the complete list of txids currently in the source's mempool, or
    /// `None` if the source cannot currently answer.
    fn get_mempool_txids(&self) -> impl SendFut<Result<Option<Vec<TxHash>>, MempoolError>>;

    /// Directly fetches one raw mempool transaction by txid.
    ///
    /// Must not call a generic `get_transaction` and must not re-fetch the full
    /// mempool txid list. Returns `Ok(None)` if the transaction disappeared
    /// between listing and fetch (a normal mempool race).
    fn get_raw_mempool_transaction(
        &self,
        txid: TxHash,
    ) -> impl SendFut<Result<Option<SerializedTransaction>, MempoolError>>;

    /// Returns the tip of the source that supplies mempool data, or `None` if the
    /// source cannot currently answer.
    fn get_mempool_source_tip(&self) -> impl SendFut<Result<Option<BlockRef>, MempoolError>>;

    /// An optional wake signal that fires when the source observes new blocks.
    ///
    /// This is only a wake hint, never a correctness guarantee; the mempool
    /// re-reads both tips after each wake. Defaults to `None` (poll-only).
    fn subscribe_to_blocks_received(&self) -> Option<tokio::sync::watch::Receiver<()>> {
        None
    }
}

/// Port: observe the current non-finalized-state epoch.
///
/// The mempool must not own or publish the non-finalized state; it only observes
/// its epoch to gate transaction-set coherence. `zaino-state` adapts its
/// `ArcSwapOption<NonFinalizedState<..>>` onto this port. Returns `None` while
/// the non-finalized state does not yet exist.
pub trait NfsEpochObserver: Clone + Send + Sync + 'static {
    /// The epoch of the currently published non-finalized snapshot, if any.
    fn current_epoch(&self) -> Option<NonFinalizedEpoch>;
}
