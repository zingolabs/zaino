//! Every index by [`IndexKind`]: one match per question (a new index = one arm in each + its
//! [`Views`] reader)

use std::future::Future;
use std::num::NonZeroUsize;
use std::pin::Pin;

use zaino_index_compact_block::{CompactBlockIndexWriter, CompactBlockReader};
use zaino_index_transparent_address::{TransparentAddressIndexWriter, TransparentAddressReader};
use zaino_index_tree_state::{TreeStateIndexWriter, TreeStateReader};
use zaino_index_utxo_set::{UtxoSetIndexWriter, UtxoSetReader};
use zaino_internal_block_hash_to_height::{BlockHashIndexWriter, BlockHashReader};
use zaino_internal_txid_location::{TxidLocationIndexWriter, TxidLocationReader};
use zaino_persistence::{
    BlockChanges, IndexKind, MapRead, OverlayView, Schema, SequenceRead, Store,
};
use zaino_primitives::types::{Block, BlockFees};
use zaino_sync::{FeeSink, IndexHandle, Subscription};
use zcash_protocol::consensus::NetworkType;

use crate::publish::Views;

#[derive(Debug, thiserror::Error)]
pub enum FoldError {
    #[error("utxo_set: {0}")]
    UtxoSet(#[from] zaino_index_utxo_set::FoldError),
    #[error("tree_state: {0}")]
    TreeState(#[from] zaino_index_tree_state::FoldError),
}

/// `kind`'s store layout on `network`
pub fn schema(kind: IndexKind, network: NetworkType) -> Schema {
    match kind {
        IndexKind::CompactBlock => zaino_index_compact_block::schema(network),
        IndexKind::UtxoSet => zaino_index_utxo_set::schema(network),
        IndexKind::BlockHash => zaino_internal_block_hash_to_height::schema(network),
        IndexKind::TreeState => zaino_index_tree_state::schema(network),
        IndexKind::TransparentAddress => zaino_index_transparent_address::schema(network),
        IndexKind::TxidLocation => zaino_internal_txid_location::schema(network),
    }
}

/// Bytes `kind`'s store buffers before committing on its own
pub fn write_buffer(kind: IndexKind) -> NonZeroUsize {
    match kind {
        IndexKind::CompactBlock => zaino_index_compact_block::WRITE_BUFFER,
        IndexKind::UtxoSet => zaino_index_utxo_set::WRITE_BUFFER,
        IndexKind::BlockHash => zaino_internal_block_hash_to_height::WRITE_BUFFER,
        IndexKind::TreeState => zaino_index_tree_state::WRITE_BUFFER,
        IndexKind::TransparentAddress => zaino_index_transparent_address::WRITE_BUFFER,
        IndexKind::TxidLocation => zaino_internal_txid_location::WRITE_BUFFER,
    }
}

/// `kind`'s own fold of `block` onto `parent` into `out`
///
/// - utxo-set → `Some` its fees, compact-block's input (`fees`): fold in [`IndexKind::ALL`] order
/// - panics: compact-block without `fees`
pub fn fold(
    kind: IndexKind,
    parent: impl SequenceRead + MapRead,
    block: &Block,
    fees: Option<&BlockFees>,
    out: &mut BlockChanges,
) -> Result<Option<BlockFees>, FoldError> {
    match kind {
        IndexKind::UtxoSet => {
            let parent = UtxoSetReader::new(parent);
            return Ok(Some(zaino_index_utxo_set::fold(&parent, block, out)?));
        }
        IndexKind::CompactBlock => {
            let parent = CompactBlockReader::new(parent);
            let fees = fees.expect("compact-block folds after utxo-set's fees");
            zaino_index_compact_block::fold(&parent, block, fees, out);
        }
        IndexKind::BlockHash => {
            let parent = BlockHashReader::new(parent);
            zaino_internal_block_hash_to_height::fold(&parent, block, out);
        }
        IndexKind::TreeState => {
            let parent = TreeStateReader::new(parent);
            zaino_index_tree_state::fold(&parent, block, out)?;
        }
        IndexKind::TransparentAddress => {
            let parent = TransparentAddressReader::new(parent);
            zaino_index_transparent_address::fold(&parent, block, out);
        }
        IndexKind::TxidLocation => {
            let parent = TxidLocationReader::new(parent);
            zaino_internal_txid_location::fold(&parent, block, out);
        }
    }
    Ok(None)
}

/// A writer's `run`, unstarted: given its finalized stream, follows it through `Shutdown`
pub(crate) type WriterRun = Box<dyn FnOnce(Subscription<Block>) -> RunFuture + Send>;
pub(crate) type RunFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// utxo-set's fees → compact-block's writer (one pairing per daemon)
pub struct FeeWiring {
    sink: Option<FeeSink>,
    compact_block: Option<Subscription<BlockFees>>,
}

impl FeeWiring {
    /// `compact_block` = its fee queue's bytes (`None` = compact-block off: fees reach no one)
    pub fn new(compact_block: Option<NonZeroUsize>) -> Self {
        let mut sink = FeeSink::new();
        let name = IndexKind::CompactBlock.name();
        let compact_block = compact_block.map(|queue| sink.subscribe(name, queue));
        Self { sink: Some(sink), compact_block }
    }
}

/// `kind`'s writer over `store` (opened with [`schema`]): its handle, its `run`
///
/// - panics: utxo-set or compact-block wired twice off one `fees`
pub fn writer<S>(
    kind: IndexKind,
    store: S,
    fees: &mut FeeWiring,
) -> (IndexHandle<S::View>, WriterRun)
where
    S: Store<View: SequenceRead + MapRead> + Send + 'static,
{
    fn boxed<F: Future<Output = ()> + Send + 'static>(run: F) -> RunFuture {
        Box::pin(run)
    }
    match kind {
        IndexKind::UtxoSet => {
            let (writer, sink) = (UtxoSetIndexWriter::new(store), fees.sink.take());
            let sink = sink.expect("one utxo-set per fee wiring");
            (writer.handle(), Box::new(move |blocks| boxed(writer.run(blocks, sink))))
        }
        IndexKind::CompactBlock => {
            let (writer, fees) = (CompactBlockIndexWriter::new(store), fees.compact_block.take());
            let fees = fees.expect("one compact-block per fee wiring, subscribed");
            (writer.handle(), Box::new(move |blocks| boxed(writer.run(blocks, fees))))
        }
        IndexKind::BlockHash => {
            let writer = BlockHashIndexWriter::new(store);
            (writer.handle(), Box::new(move |blocks| boxed(writer.run(blocks))))
        }
        IndexKind::TreeState => {
            let writer = TreeStateIndexWriter::new(store);
            (writer.handle(), Box::new(move |blocks| boxed(writer.run(blocks))))
        }
        IndexKind::TransparentAddress => {
            let writer = TransparentAddressIndexWriter::new(store);
            (writer.handle(), Box::new(move |blocks| boxed(writer.run(blocks))))
        }
        IndexKind::TxidLocation => {
            let writer = TxidLocationIndexWriter::new(store);
            (writer.handle(), Box::new(move |blocks| boxed(writer.run(blocks))))
        }
    }
}

impl<V: SequenceRead> Views<V> {
    pub fn compact_block(&self) -> Option<CompactBlockReader<OverlayView<V>>> {
        let view = self.view(IndexKind::CompactBlock)?;
        Some(CompactBlockReader::new(view))
    }

    pub fn tree_state(&self) -> Option<TreeStateReader<OverlayView<V>>> {
        Some(TreeStateReader::new(self.view(IndexKind::TreeState)?))
    }
}

impl<V: MapRead> Views<V> {
    pub fn block_hash(&self) -> Option<BlockHashReader<OverlayView<V>>> {
        Some(BlockHashReader::new(self.view(IndexKind::BlockHash)?))
    }

    pub fn txid_location(&self) -> Option<TxidLocationReader<OverlayView<V>>> {
        Some(TxidLocationReader::new(self.view(IndexKind::TxidLocation)?))
    }

    pub fn transparent_address(&self) -> Option<TransparentAddressReader<OverlayView<V>>> {
        let view = self.view(IndexKind::TransparentAddress)?;
        Some(TransparentAddressReader::new(view))
    }
}

impl<V: SequenceRead + MapRead> Views<V> {
    /// Fold parents + a peer quorum's prevouts (no route reads utxo-set)
    pub fn utxo_set(&self) -> Option<UtxoSetReader<OverlayView<V>>> {
        Some(UtxoSetReader::new(self.view(IndexKind::UtxoSet)?))
    }
}
