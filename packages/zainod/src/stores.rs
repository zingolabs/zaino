//! Each index's store schema: its kind + its crate's `FORMAT` and `TABLES` (boot opens, `verify`
//! scrubs by it), and the crate's `WRITE_BUFFER` boot commits at

use std::num::NonZeroUsize;

use zaino_index_compact_block as compact_block;
use zaino_index_transparent_address as transparent_address;
use zaino_index_tree_state as tree_state;
use zaino_internal_block_hash_to_height as block_hash;
use zaino_internal_value_balance as value_balance;
use zaino_persistence::{IndexKind, Schema};
use zcash_protocol::consensus::NetworkType;

pub(crate) fn schema(kind: IndexKind, network: NetworkType) -> (Schema, NonZeroUsize) {
    let (format, tables, write_buffer) = match kind {
        IndexKind::CompactBlock => {
            (compact_block::FORMAT, compact_block::TABLES, compact_block::WRITE_BUFFER)
        }
        IndexKind::ValueBalance => {
            (value_balance::FORMAT, value_balance::TABLES, value_balance::WRITE_BUFFER)
        }
        IndexKind::BlockHash => (block_hash::FORMAT, block_hash::TABLES, block_hash::WRITE_BUFFER),
        IndexKind::TreeState => (tree_state::FORMAT, tree_state::TABLES, tree_state::WRITE_BUFFER),
        IndexKind::TransparentAddress => (
            transparent_address::FORMAT,
            transparent_address::TABLES,
            transparent_address::WRITE_BUFFER,
        ),
    };
    (Schema::new(kind, format, network, tables), write_buffer)
}
