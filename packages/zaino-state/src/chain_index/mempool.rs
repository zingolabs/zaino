//! ChainIndex's side of the mempool boundary.
//!
//! What the mempool subsystem needs from `zaino-state`, and how it gets it.
//!
//! Named for the boundary, not for what it currently holds. These are adapters
//! today, but a suffix saying so would rot the first time a bound or a
//! conversion joined them — which is exactly how `_ports` came to mean two
//! different things in this crate (`source_ports` really does hold a port).
//! Each subsystem extracted from ChainIndex gets a module named this way, so
//! the name survives the contents changing.
//!
//! The mempool reads the validator through [`zaino_source`]'s ports, which
//! ChainIndex's source already answers, so nothing here translates validator
//! data. What the two adapters below supply is the part `zaino-source` has
//! nothing to say about, because it is a fact about *Zaino's* state rather than
//! the validator's:
//!
//! - [`MempoolSourceAdapter`] supplies the block-arrival wake, which must come
//!   from ChainIndex's sync loop rather than from the source. Its port impls
//!   forward the validator questions untouched.
//! - [`NfsEpochAdapter`] exposes the non-finalized state's epoch, which is what
//!   the coherence layer freezes and thaws against.
//!
//! Dependencies point inward: these adapters know about the mempool crates; the
//! mempool crates never name a `zaino-state` type.

use std::sync::Arc;

use arc_swap::ArcSwapOption;

use crate::chain_index::non_finalised_state::NonFinalizedState;
use crate::chain_index::source::BlockchainSource;

/// The tip-agnostic core mempool the ChainIndex owns.
///
/// Serves the live, never-frozen reads — `getrawmempool`, `getmempoolinfo`,
/// `GetMempoolTx` — which must keep answering across a tip transition.
pub(crate) type ChainIndexMempool<Source> =
    zaino_mempool_service::MempoolService<MempoolSourceAdapter<Source>>;

/// The tip-aware coherence layer the ChainIndex owns.
///
/// Wraps the core's read handle and this crate's non-finalized-state epoch
/// observer to serve the reads that place a transaction relative to a tip —
/// `get_raw_transaction`, `get_transaction_status`, and the coherent
/// raw-transaction stream.
pub(crate) type ChainIndexCoherence<Source> = zaino_mempool_service::CoherenceService<
    zaino_mempool_service::MempoolSubscriber,
    NfsEpochAdapter<Source>,
>;

/// Wraps ChainIndex's source to give the mempool a block-arrival wake.
///
/// Every mempool data port forwards to the wrapped source untouched; those impls
/// exist only because a trait impl does not travel through a wrapper on its own.
/// The one thing this adds is `SubscribeBlocks`.
///
/// It has to. `ValidatorSource` has no push path in production — reaching the
/// validator over request/response gives none — so without a wake the mempool's
/// addition latency would always be a full poll interval. The ChainIndex sync
/// loop *does* know when a block landed, so it fires this signal, and the
/// mempool gets a block-driven push path the source cannot offer.
///
/// This is a wake hint and nothing more. The tip is re-read from the source on
/// every tick regardless, so a missed or spurious signal costs latency, never
/// correctness.
#[derive(Clone)]
pub(crate) struct MempoolSourceAdapter<S> {
    source: S,
    block_wake: tokio::sync::watch::Receiver<()>,
}

impl<S> MempoolSourceAdapter<S> {
    pub(crate) fn new(source: S, block_wake: tokio::sync::watch::Receiver<()>) -> Self {
        Self { source, block_wake }
    }
}

impl<S: BlockchainSource> zaino_source::GetMempoolTxids for MempoolSourceAdapter<S> {
    fn get_mempool_txids(
        &self,
    ) -> impl std::future::Future<
        Output = Result<
            Vec<zaino_primitives::types::TransactionId>,
            zaino_source::QueryError<zaino_source::GetMempoolTxidsError>,
        >,
    > + Send {
        self.source.get_mempool_txids()
    }
}

impl<S: BlockchainSource> zaino_source::GetMempoolMetadata for MempoolSourceAdapter<S> {
    fn get_mempool_metadata(
        &self,
    ) -> impl std::future::Future<
        Output = Result<
            Vec<zaino_source::MempoolTxMeta>,
            zaino_source::QueryError<zaino_source::GetMempoolMetadataError>,
        >,
    > + Send {
        self.source.get_mempool_metadata()
    }
}

impl<S: BlockchainSource> zaino_source::GetRawMempoolTransaction for MempoolSourceAdapter<S> {
    fn get_raw_mempool_transaction(
        &self,
        txid: zaino_primitives::types::TransactionId,
    ) -> impl std::future::Future<
        Output = Result<
            Vec<u8>,
            zaino_source::QueryError<zaino_source::GetRawMempoolTransactionError>,
        >,
    > + Send {
        self.source.get_raw_mempool_transaction(txid)
    }
}

impl<S: BlockchainSource> zaino_source::GetMempoolSourceTip for MempoolSourceAdapter<S> {
    fn get_mempool_source_tip(
        &self,
    ) -> impl std::future::Future<
        Output = Result<
            (
                zaino_primitives::types::BlockHash,
                zaino_primitives::types::Height,
            ),
            zaino_source::QueryError<std::convert::Infallible>,
        >,
    > + Send {
        self.source.get_mempool_source_tip()
    }
}

impl<S: BlockchainSource> zaino_source::SubscribeBlocks for MempoolSourceAdapter<S> {
    fn subscribe_to_blocks_received(&self) -> Option<tokio::sync::watch::Receiver<()>> {
        Some(self.block_wake.clone())
    }
}

/// Presents the ChainIndex's non-finalized state as the mempool's epoch
/// observer.
///
/// Reads the *same* `ArcSwapOption` the ChainIndex publishes into, so the
/// mempool observes exactly the epoch the rest of the ChainIndex serves. Holding
/// a separate copy would let the two drift, and the coherence layer would be
/// freezing against a tip nobody was being served.
///
/// It never owns or mutates the non-finalized state — it only reads its epoch.
pub(crate) struct NfsEpochAdapter<Source: BlockchainSource> {
    non_finalized_state: Arc<ArcSwapOption<NonFinalizedState<Source>>>,
    /// Fired by the ChainIndex sync loop on each publication.
    epoch_wake: tokio::sync::watch::Receiver<()>,
}

impl<Source: BlockchainSource> NfsEpochAdapter<Source> {
    /// Wrap the ChainIndex's shared non-finalized-state handle and its
    /// publication signal.
    pub(crate) fn new(
        non_finalized_state: Arc<ArcSwapOption<NonFinalizedState<Source>>>,
        epoch_wake: tokio::sync::watch::Receiver<()>,
    ) -> Self {
        Self {
            non_finalized_state,
            epoch_wake,
        }
    }
}

/// Written out rather than derived: `derive(Clone)` would demand
/// `Source: Clone`, which the adapter does not need — it holds only shared
/// handles.
impl<Source: BlockchainSource> Clone for NfsEpochAdapter<Source> {
    fn clone(&self) -> Self {
        Self {
            non_finalized_state: Arc::clone(&self.non_finalized_state),
            epoch_wake: self.epoch_wake.clone(),
        }
    }
}

impl<Source: BlockchainSource> zaino_mempool::NfsEpochObserver for NfsEpochAdapter<Source> {
    fn current_epoch(&self) -> Option<zaino_mempool::NonFinalizedEpoch> {
        let non_finalized_state = self.non_finalized_state.load_full()?;
        Some(non_finalized_state.get_snapshot().epoch())
    }

    fn subscribe_epoch_changes(&self) -> Option<tokio::sync::watch::Receiver<()>> {
        Some(self.epoch_wake.clone())
    }
}
