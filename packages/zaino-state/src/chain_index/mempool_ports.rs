//! Adapters wiring `zaino-state` into the `zaino-mempool` ports.
//!
//! Hexagonal boundary: `zaino-mempool` (the core) defines the
//! [`MempoolSource`](zaino_mempool::MempoolSource) and
//! [`NfsEpochObserver`](zaino_mempool::NfsEpochObserver) ports, and this module
//! supplies the concrete adapters over `zaino-state`'s [`BlockchainSource`] and
//! non-finalized state. Dependencies point inward: these adapters know about the
//! mempool core; the core never names a `zaino-state` type.

use std::sync::Arc;

use arc_swap::ArcSwapOption;

use crate::chain_index::non_finalised_state::NonFinalizedState;
use crate::chain_index::source::BlockchainSource;
use crate::chain_index::types::BlockIndex;

/// The concrete mempool service the ChainIndex owns: the `zaino-mempool` service
/// driven by this crate's source and non-finalized-state adapters.
pub(crate) type ChainIndexMempool<Source> =
    zaino_mempool::MempoolService<MempoolSourceAdapter<Source>, NfsEpochAdapter<Source>>;

/// Maps a `zaino-state` [`BlockchainSourceError`](crate::chain_index::source::BlockchainSourceError)
/// (or any boxable adapter error) into a [`zaino_mempool::MempoolError`].
fn to_mempool_error<E>(error: E) -> zaino_mempool::MempoolError
where
    E: std::error::Error + Send + Sync + 'static,
{
    zaino_mempool::MempoolError::source(error)
}

/// Converts an internal `(height, hash)` [`BlockIndex`] into the mempool port's
/// [`BlockRef`](zaino_mempool::BlockRef).
fn block_index_to_ref(block_index: BlockIndex) -> zaino_mempool::BlockRef {
    zaino_mempool::BlockRef {
        height: block_index.height.into(),
        hash: block_index.hash.into(),
    }
}

/// Adapter presenting any [`BlockchainSource`] as the mempool's
/// [`MempoolSource`](zaino_mempool::MempoolSource) port.
///
/// A local newtype so the foreign-trait impl is orphan-safe and generic over the
/// concrete source (`ValidatorConnector` in production, the mock in tests).
#[derive(Clone)]
pub(crate) struct MempoolSourceAdapter<S>(pub(crate) S);

impl<S: BlockchainSource> zaino_mempool::MempoolSource for MempoolSourceAdapter<S> {
    async fn get_mempool_metadata(
        &self,
    ) -> Result<Option<Vec<zaino_mempool::MempoolTxMeta>>, zaino_mempool::MempoolError> {
        self.0
            .get_mempool_metadata()
            .await
            .map_err(to_mempool_error)
    }

    async fn get_raw_mempool_transaction(
        &self,
        txid: zebra_chain::transaction::Hash,
    ) -> Result<Option<zebra_chain::transaction::SerializedTransaction>, zaino_mempool::MempoolError>
    {
        self.0
            .get_raw_mempool_transaction(txid)
            .await
            .map_err(to_mempool_error)
    }

    async fn get_mempool_source_tip(
        &self,
    ) -> Result<Option<zaino_mempool::BlockRef>, zaino_mempool::MempoolError> {
        Ok(self
            .0
            .get_mempool_source_tip()
            .await
            .map_err(to_mempool_error)?
            .map(block_index_to_ref))
    }

    fn subscribe_to_blocks_received(&self) -> Option<tokio::sync::watch::Receiver<()>> {
        self.0.subscribe_to_blocks_received()
    }
}

/// Adapter presenting the ChainIndex's non-finalized state as the mempool's
/// [`NfsEpochObserver`](zaino_mempool::NfsEpochObserver) port.
///
/// Wraps the *same* `ArcSwapOption` the ChainIndex publishes into, so the mempool
/// observes exactly the epoch the rest of the ChainIndex serves. It never owns or
/// mutates the non-finalized state — it only reads its epoch.
pub(crate) struct NfsEpochAdapter<Source: BlockchainSource> {
    non_finalized_state: Arc<ArcSwapOption<NonFinalizedState<Source>>>,
}

impl<Source: BlockchainSource> NfsEpochAdapter<Source> {
    /// Wrap the ChainIndex's shared non-finalized-state handle.
    pub(crate) fn new(non_finalized_state: Arc<ArcSwapOption<NonFinalizedState<Source>>>) -> Self {
        Self {
            non_finalized_state,
        }
    }
}

impl<Source: BlockchainSource> Clone for NfsEpochAdapter<Source> {
    fn clone(&self) -> Self {
        Self {
            non_finalized_state: Arc::clone(&self.non_finalized_state),
        }
    }
}

impl<Source: BlockchainSource> zaino_mempool::NfsEpochObserver for NfsEpochAdapter<Source> {
    fn current_epoch(&self) -> Option<zaino_mempool::NonFinalizedEpoch> {
        let non_finalized_state = self.non_finalized_state.load_full()?;
        Some(non_finalized_state.get_snapshot().epoch())
    }
}
