//! Ports required and offered by the mempool subsystem.
//!
//! These traits are *consumer-defined*: each layer declares exactly what it needs
//! from the outside world, and the wiring crate (`zaino-state`) supplies adapters.
//! See the crate-level docs for the hexagonal rationale.
//!
//! - [`MempoolSource`] — the outbound port the tip-agnostic core needs (validator
//!   mempool data + the tip that data was read at).
//! - [`Mempool`] — the inbound port the core *offers*: the tip-agnostic read model
//!   plus the [`MempoolUpdate`] change feed. The tip-aware coherence layer
//!   consumes it.
//! - [`NfsEpochObserver`] / [`TipAwareMempool`] — gated behind `tip_aware_mempool`:
//!   the NS-epoch observer the coherence layer needs, and the coherent read/stream
//!   port it offers.

use std::sync::Arc;

use tokio::sync::broadcast;
use zebra_chain::{
    block::{Hash as BlockHash, Height},
    transaction::{Hash as TxHash, SerializedTransaction},
};

use crate::snapshot::MempoolSnapshot;
use crate::update::MempoolUpdate;
use crate::{MempoolError, SendFut};

/// A minimal `(height, hash)` reference to a block.
///
/// The mempool subsystem uses this instead of `zaino-state`'s `BlockIndex` so it
/// does not depend on `zaino-state`. Adapters map their richer tip types onto it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockRef {
    /// The block's height.
    pub height: Height,
    /// The block's hash.
    pub hash: BlockHash,
}

/// Per-transaction mempool metadata, as reported by the source's mempool
/// listing (`getrawmempool verbose` for the validator).
///
/// `entry_height` is the source's authoritative chain tip height when the
/// transaction entered the mempool — Zebra's `VerifiedUnminedTx.height` /
/// zcashd's `nHeight`. Sourcing it from the validator keeps Zaino's mempool
/// entries protocol-correct rather than derived. `entry_time` is the unix time
/// (seconds) it entered, when the source reports one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MempoolTxMeta {
    /// The transaction's id.
    pub txid: TxHash,
    /// Chain tip height when the transaction entered the source's mempool.
    pub entry_height: Height,
    /// Unix time (seconds) the transaction entered the mempool, if known.
    pub entry_time: Option<i64>,
}

/// Outbound port: the source of mempool data (txids, raw transactions, and the
/// tip of the source that supplies them).
///
/// For the State backend this must be backed by the *same* fetcher that serves
/// mempool txids (Zebra's JSON-RPC `mempool_fetcher`), **not** `ReadStateService`,
/// so the observed tip and the mempool data come from one consistent source. The
/// core tags every published set with [`get_mempool_source_tip`](Self::get_mempool_source_tip)
/// precisely so the coherence layer can compare it against the NS tip *without*
/// re-fetching — a comparison that is only sound because the tag and the data are
/// this single-source pair.
///
/// Implementations must be cheap to `clone` — the mempool core clones the source
/// to fan out bounded, concurrent raw-transaction fetches.
pub trait MempoolSource: Clone + Send + Sync + 'static {
    /// Returns just the txids currently in the source's mempool, or `None` if the
    /// source cannot currently answer.
    ///
    /// This is the cheap per-poll listing used to diff the mempool set. Heights
    /// for *new* transactions are then obtained from [`Self::get_mempool_metadata`],
    /// which is only fetched when the diff shows additions.
    fn get_mempool_txids(&self) -> impl SendFut<Result<Option<Vec<TxHash>>, MempoolError>>;

    /// Returns per-transaction metadata for the source's entire current mempool,
    /// or `None` if the source cannot currently answer.
    ///
    /// Each entry carries its txid *and* the validator's tip-at-entry height (see
    /// [`MempoolTxMeta`]), so the read model can stamp entries protocol-correctly.
    /// This is the heavier verbose listing; the mempool fetches it only when
    /// [`Self::get_mempool_txids`] shows additions.
    ///
    /// # Cost
    ///
    /// On a real validator this is a **whole-mempool walk** (`getrawmempool
    /// verbose`), and it is the dominant per-poll cost of the mempool subsystem.
    /// It is retained deliberately: the tip-at-entry height it returns is a
    /// protocol field the validator owns, and Zaino must not substitute a locally
    /// derived value (see the "explicitly not doing `entry_height` derivation"
    /// note in `docs/audit.md`). The mitigation is coalescing —
    /// [`metadata_min_interval`](crate::config::MempoolConfig::metadata_min_interval)
    /// bounds how often the walk runs and defers additions between walks — not
    /// removal; a metadata-by-txid source method would remove it, and is the lead
    /// ask of the drafted upstream (Zebra) issue.
    fn get_mempool_metadata(
        &self,
    ) -> impl SendFut<Result<Option<Vec<MempoolTxMeta>>, MempoolError>>;

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
    ///
    /// The core reads this on every poll and stamps it onto the published
    /// snapshot ([`MempoolSnapshot::source_tip`](crate::snapshot::MempoolSnapshot::source_tip)).
    fn get_mempool_source_tip(&self) -> impl SendFut<Result<Option<BlockRef>, MempoolError>>;

    /// An optional wake signal that fires when the source observes new blocks.
    ///
    /// This is only a wake hint, never a correctness guarantee; the mempool
    /// re-reads the tip after each wake. Defaults to `None` (poll-only).
    fn subscribe_to_blocks_received(&self) -> Option<tokio::sync::watch::Receiver<()>> {
        None
    }
}

/// Inbound port: the tip-agnostic mempool read model the core offers.
///
/// The core is always live and never freezes; this port exposes its current
/// [`MempoolSnapshot`] and the [`MempoolUpdate`] change feed. The optional
/// coherence layer consumes it (generic over `M: Mempool`) to build the
/// tip-coherent view; it carries no chain-tip knowledge itself.
pub trait Mempool: Clone + Send + Sync + 'static {
    /// The current tip-agnostic snapshot (tagged with its `source_tip`). Always
    /// the authoritative latest set; this is the resync source after a lag.
    fn current(&self) -> Arc<MempoolSnapshot>;

    /// Subscribe to the bounded mempool change feed.
    ///
    /// **Subscribe before reading [`current`](Self::current)**, and on
    /// `RecvError::Lagged` **resync from `current`** — see the
    /// [`update`](crate::update) module for the full consistency contract. The
    /// feed is bounded (lossless at the level of *state*, not every delta), so it
    /// scales to many consumers without unbounded buffering.
    fn subscribe_updates(&self) -> broadcast::Receiver<MempoolUpdate>;
}

/// A stable identifier for a published non-finalized-state snapshot.
///
/// `generation` increments when the publisher's best tip *changes*, not on every
/// republication: the sync loop republishes each iteration (to trim finalized
/// blocks and so on) even when the tip has not moved, and bumping the generation
/// on those no-op republishes would churn the epoch every cycle and defeat the
/// coherence layer's agreement check. Keying it to tip changes gives a stable
/// epoch for a stable tip while still distinguishing successive tips — including
/// same-height reorgs, which change the tip hash. The coherence layer keys on
/// the whole epoch (generation *and* tip); hash-only matching would be weaker.
#[cfg(feature = "tip_aware_mempool")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NonFinalizedEpoch {
    /// Monotonic publication generation of the non-finalized snapshot.
    pub generation: u64,
    /// The best (tip) block of the non-finalized snapshot.
    pub best_tip: BlockRef,
}

/// Outbound port (coherence layer): observe the current non-finalized-state epoch.
///
/// The mempool must not own or publish the non-finalized state; the coherence
/// layer only observes its epoch to gate transaction-set coherence. `zaino-state`
/// adapts its `ArcSwapOption<NonFinalizedState<..>>` onto this port. Returns
/// `None` while the non-finalized state does not yet exist.
#[cfg(feature = "tip_aware_mempool")]
pub trait NfsEpochObserver: Clone + Send + Sync + 'static {
    /// The epoch of the currently published non-finalized snapshot, if any.
    fn current_epoch(&self) -> Option<NonFinalizedEpoch>;

    /// An optional wake signal that fires when a new non-finalized snapshot is
    /// published.
    ///
    /// Without it the coherence layer only notices an NS advance on its next
    /// poll tick, so every block is followed by a blackout of that length in
    /// which tip-coherent reads are frozen. Like
    /// [`MempoolSource::subscribe_to_blocks_received`] this is a wake hint, not a
    /// correctness guarantee — the epoch is re-read on every reconcile — so
    /// implementations may return `None` and rely on the tick.
    fn subscribe_epoch_changes(&self) -> Option<tokio::sync::watch::Receiver<()>> {
        None
    }
}

/// A placeholder [`NfsEpochObserver`] for validator-only coherence.
///
/// It is never consulted — validator-only mode synthesizes the epoch from the
/// validator tip — but supplies a concrete observer type for the validator-only
/// coherence constructor.
#[cfg(feature = "tip_aware_mempool")]
#[derive(Debug, Clone, Copy, Default)]
pub struct NoNfs;

#[cfg(feature = "tip_aware_mempool")]
impl NfsEpochObserver for NoNfs {
    fn current_epoch(&self) -> Option<NonFinalizedEpoch> {
        None
    }
}

/// Why a tip-coherent transaction stream ended early.
///
/// The stream's normal ending — the chain tip moved on — is signalled by the
/// stream simply finishing. This type exists so the *abnormal* ending is not
/// mistaken for it.
#[cfg(feature = "tip_aware_mempool")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MempoolStreamError {
    /// The consumer fell behind the bounded event feed, so transactions were
    /// missed. The set delivered so far is incomplete; re-open the stream
    /// against a fresh snapshot.
    #[error("mempool stream lagged behind the event feed; {missed} events missed")]
    Lagged {
        /// Number of events the consumer fell behind by.
        missed: u64,
    },
}

/// Inbound port (coherence layer): the tip-coherent mempool read model.
///
/// Offered by the coherence service. It wraps a [`Mempool`] core and an
/// [`NfsEpochObserver`] and publishes a [`CoherentSnapshot`](crate::tip::CoherentSnapshot)
/// that combined ChainIndex reads consult so they only serve the mempool when it
/// is coherent with the caller's NS snapshot.
#[cfg(feature = "tip_aware_mempool")]
pub trait TipAwareMempool: Clone + Send + Sync + 'static {
    /// The current coherent view (its `mode` / `valid_for` say whether, and for
    /// which NS epoch, the wrapped set may be served).
    fn coherent_snapshot(&self) -> Arc<crate::tip::CoherentSnapshot>;

    /// A stream of serialized mempool transactions coherent with a single chain
    /// tip, which **closes when the tip changes**.
    ///
    /// Yields the current coherent set's transactions, then each subsequently
    /// added transaction, and ends once the view becomes live for a *new* epoch
    /// (the validator and NS tips re-agreed at a new tip) or the service closes. A
    /// transient freeze does *not* end the stream — the last coherent set stays
    /// readable until the tips re-agree, so the caller's next call (with the new
    /// tip) finds a matching, live view. Returns `None` immediately if
    /// `expected_epoch` does not match the current coherent view — the caller's
    /// chain tip is stale and should re-snapshot.
    ///
    /// Items are `Result`s: a consumer that falls behind the bounded event feed
    /// receives [`MempoolStreamError::Lagged`] and the stream ends. Ending
    /// silently there would be indistinguishable from a normal tip-change close,
    /// so the client would believe it had received the whole mempool when
    /// transactions had in fact been skipped.
    ///
    /// This is the single, ready-made "stream the mempool until the tip moves"
    /// loop; the caller just drives it with `StreamExt::next`.
    fn stream_transactions_until_tip_change(
        &self,
        expected_epoch: Option<NonFinalizedEpoch>,
    ) -> Option<impl futures::Stream<Item = Result<bytes::Bytes, MempoolStreamError>> + Send>;
}
