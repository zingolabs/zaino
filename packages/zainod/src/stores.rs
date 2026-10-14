//! Each index's store schema: its kind + its crate's `FORMAT` and `TABLES` (boot opens, `verify`
//! scrubs by it), and the crate's `WRITE_BUFFER` boot commits at

use std::num::NonZeroUsize;

use zaino_persistence::{IndexKind, Schema};
use zcash_protocol::consensus::NetworkType;

pub(crate) fn schema(kind: IndexKind, network: NetworkType) -> (Schema, NonZeroUsize) {
    let (format, tables, write_buffer) = match kind {
        IndexKind::CompactBlock => (
            zaino_index_compact_block::FORMAT,
            zaino_index_compact_block::TABLES,
            zaino_index_compact_block::WRITE_BUFFER,
        ),
        IndexKind::UtxoSet => (
            zaino_index_utxo_set::FORMAT,
            zaino_index_utxo_set::TABLES,
            zaino_index_utxo_set::WRITE_BUFFER,
        ),
        IndexKind::BlockHash => (
            zaino_internal_block_hash_to_height::FORMAT,
            zaino_internal_block_hash_to_height::TABLES,
            zaino_internal_block_hash_to_height::WRITE_BUFFER,
        ),
        IndexKind::TreeState => (
            zaino_index_tree_state::FORMAT,
            zaino_index_tree_state::TABLES,
            zaino_index_tree_state::WRITE_BUFFER,
        ),
        IndexKind::TransparentAddress => (
            zaino_index_transparent_address::FORMAT,
            zaino_index_transparent_address::TABLES,
            zaino_index_transparent_address::WRITE_BUFFER,
        ),
    };
    (Schema::new(kind, format, network, tables), write_buffer)
}
