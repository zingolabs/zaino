//! Ports required and offered by the mempool subsystem.
//!
//! The core reads the validator through `zaino-source`'s ports — one trait per
//! question — and names the subset it needs as [`MempoolSource`], in this crate,
//! because that subset is a requirement of *this* consumer rather than a
//! capability of `zaino-source`. Everything the mempool needs that `zaino-source`
//! does not describe is a port defined here, and the wiring crate
//! (`zaino-state`) supplies the adapter.
//!
//! - [`MempoolSource`] — the validator questions the tip-agnostic core asks:
//!   mempool data plus the tip that data was read at.
//! - [`Mempool`] — the inbound port the core *offers*: the tip-agnostic read model
//!   plus the [`MempoolUpdate`] change feed. The tip-aware coherence layer
//!   consumes it.
//! - [`NfsEpochObserver`] / [`TipAwareMempool`] — gated behind `tip_aware_mempool`:
//!   the NS-epoch observer the coherence layer needs, and the coherent read/stream
//!   port it offers. These have no `zaino-source` equivalent: they describe
//!   Zaino's own non-finalized state, not the validator's.

use std::sync::Arc;

use tokio::sync::broadcast;
use zaino_primitives::types::BlockRef;

use crate::snapshot::MempoolSnapshot;
use crate::update::MempoolUpdate;

/// A transport that can source a validator's mempool coherently.
///
/// A consumer-defined bound over `zaino-source`'s ports: it states a requirement
/// of this crate, not a capability of `zaino-source`, which should not have to
/// know who its consumers are.
///
/// Named for the capability it represents rather than for its place in the
/// hexagon. A bound reads best as *what a type satisfying it can do* —
/// `impl<S: MempoolSource>` says the thing can source a mempool, where
/// `MempoolPorts` said only that it was a bag of ports. `zaino-state`'s
/// `ChainIndexSourcePorts` is the same construct under the older convention;
/// that crate is transitional wiring being retired as its subsystems move into
/// their own crates, and each one that lands takes the capability name.
///
/// Nothing implements this directly — the blanket impl below applies it to any
/// type that answers all of the questions, so an adapter earns the bound by
/// implementing the ports it can serve.
///
/// # The single-source rule
///
/// Every *validator* port in this bound — the four data and tip reads — must be
/// answered by the **same** transport. The core tags each published set with
/// [`get_mempool_source_tip`](zaino_source::GetMempoolSourceTip::get_mempool_source_tip)
/// so the coherence layer can judge the set's coherence without re-fetching it,
/// and that comparison is only sound for a single-source pair. `ZebraValidator`
/// upholds this by routing all four to JSON-RPC; see
/// [`GetMempoolSourceTip`](zaino_source::GetMempoolSourceTip)'s documentation.
///
/// [`SubscribeBlocks`](zaino_source::SubscribeBlocks) is exempt, and is the
/// reason this is a capability rather than a plain source: it is a wake hint,
/// supplied by whoever knows a block landed — in production `zaino-state`'s sync
/// loop, not the validator — and a missed or spurious signal costs latency, not
/// correctness.
///
/// `Clone` is required because the core clones the source to fan out bounded,
/// concurrent raw-transaction fetches, so implementations must be cheap to clone.
pub trait MempoolSource:
    zaino_source::GetMempoolTxids
    + zaino_source::GetMempoolMetadata
    + zaino_source::GetRawMempoolTransaction
    + zaino_source::GetMempoolSourceTip
    + zaino_source::SubscribeBlocks
    + Clone
    + Send
    + Sync
    + 'static
{
}

impl<T> MempoolSource for T where
    T: zaino_source::GetMempoolTxids
        + zaino_source::GetMempoolMetadata
        + zaino_source::GetRawMempoolTransaction
        + zaino_source::GetMempoolSourceTip
        + zaino_source::SubscribeBlocks
        + Clone
        + Send
        + Sync
        + 'static
{
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
    /// [`SubscribeBlocks`](zaino_source::SubscribeBlocks) this is a wake hint, not a
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
