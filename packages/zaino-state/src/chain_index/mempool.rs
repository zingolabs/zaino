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
//! - [`ChainHeadEpochAdapter`] exposes the chain head's epoch, which is what
//!   the coherence layer freezes and thaws against.
//!
//! Dependencies point inward: these adapters know about the mempool crates; the
//! mempool crates never name a `zaino-state` type.

use crate::chain_index::source::BlockchainSource;

/// The tip-agnostic core mempool the ChainIndex owns.
///
/// Serves the live, never-frozen reads — `getrawmempool`, `getmempoolinfo`,
/// `GetMempoolTx` — which must keep answering across a tip transition.
pub(crate) type ChainIndexMempool<Source> =
    zaino_mempool_service::MempoolService<MempoolSourceAdapter<Source>>;

/// The tip-aware coherence layer the ChainIndex owns.
///
/// Wraps the core's read handle and the chain head's epoch observer to serve
/// the reads that place a transaction relative to a tip —
/// `get_raw_transaction`, `get_transaction_status`, and the coherent
/// raw-transaction stream.
pub(crate) type ChainIndexCoherence = zaino_mempool_service::CoherenceService<
    zaino_mempool_service::MempoolSubscriber,
    ChainHeadEpochAdapter,
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

impl<S: BlockchainSource> zaino_source::OneShotGetMempoolMetadata for MempoolSourceAdapter<S> {
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

/// Presents the chain head's epoch as the mempool's epoch observer.
///
/// Reads the *same* subscriber the rest of the ChainIndex serves snapshots
/// from, so the coherence layer observes exactly the epoch callers are being
/// answered against. A separate view of the chain would let the two drift, and
/// coherence would be freezing against a tip nobody was being served.
///
/// It never drives the chain head — it only observes its epoch.
#[derive(Clone)]
pub(crate) struct ChainHeadEpochAdapter {
    chain_head: zaino_chain_head_service::ChainHeadSubscriber,
    /// Fires on each chain head epoch change.
    epoch_wake: tokio::sync::watch::Receiver<()>,
}

impl ChainHeadEpochAdapter {
    /// Wrap a chain head subscriber, and start the relay that turns its epoch
    /// feed into the unit wake this port is defined in terms of.
    ///
    /// The relay exists purely to bridge two watch channels of different item
    /// types; `watch::Receiver` cannot be mapped in place. It is worth a task
    /// because without a wake the coherence layer only notices a tip change on
    /// its next poll tick, so every block would be followed by a blackout of
    /// that length in which tip-coherent reads are frozen.
    ///
    /// It ends when the token is cancelled or the chain head stops publishing —
    /// **not** when the adapter is dropped, which it does not observe. The
    /// token is a child of ChainIndex's, so its lifetime is bounded by the
    /// thing that spawned it.
    ///
    /// It drops the epoch it reads: the value is re-read from
    /// [`current_epoch`](zaino_mempool::NfsEpochObserver::current_epoch) on
    /// every reconcile, so this is a hint and never a source of truth.
    pub(crate) fn spawn(
        chain_head: zaino_chain_head_service::ChainHeadSubscriber,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Self {
        let (epoch_signal, epoch_wake) = tokio::sync::watch::channel(());
        let mut updates = zaino_chain_head::ChainHeadBlockService::subscribe_updates(&chain_head);

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = cancel_token.cancelled() => break,
                    changed = updates.changed() => {
                        if changed.is_err() {
                            // The chain head is gone; there will be no more
                            // epochs to relay.
                            break;
                        }
                        // A lost send costs latency, never correctness.
                        let _ = epoch_signal.send(());
                    }
                }
            }
        });

        Self {
            chain_head,
            epoch_wake,
        }
    }
}

impl zaino_mempool::NfsEpochObserver for ChainHeadEpochAdapter {
    fn current_epoch(&self) -> Option<zaino_primitives::types::ChainStateEpoch> {
        use zaino_chain_head::ChainHeadSnapshot as _;

        Some(zaino_chain_head::ChainHeadBlockService::current(&self.chain_head).epoch())
    }

    fn subscribe_epoch_changes(&self) -> Option<tokio::sync::watch::Receiver<()>> {
        Some(self.epoch_wake.clone())
    }
}
