//! Per-`IndexKind` registry: each index's store [`schema`] + its [`Views`] reader (fold: `fold.rs`)

use zaino_index_compact_block::CompactBlockReader;
use zaino_index_transparent_address::TransparentAddressReader;
use zaino_index_tree_state::TreeStateReader;
use zaino_index_utxo_set::UtxoSetReader;
use zaino_internal_block_hash_to_height::BlockHashReader;
use zaino_persistence::{IndexKind, MapRead, OverlayView, Schema, SequenceRead};
use zcash_protocol::consensus::NetworkType;

use crate::publish::Views;

/// `kind`'s store layout on `network` (its index crate's `schema`)
pub fn schema(kind: IndexKind, network: NetworkType) -> Schema {
    match kind {
        IndexKind::CompactBlock => zaino_index_compact_block::schema(network),
        IndexKind::UtxoSet => zaino_index_utxo_set::schema(network),
        IndexKind::BlockHash => zaino_internal_block_hash_to_height::schema(network),
        IndexKind::TreeState => zaino_index_tree_state::schema(network),
        IndexKind::TransparentAddress => zaino_index_transparent_address::schema(network),
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
