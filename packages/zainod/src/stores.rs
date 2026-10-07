//! Each index's store schema: its kind + its crate's `FORMAT` and `TABLES` (boot opens, `verify`
//! scrubs by it)

use zaino_index_compact_block as compact_block;
use zaino_index_transparent_address as transparent_address;
use zaino_index_tree_state as tree_state;
use zaino_internal_block_hash_to_height as block_hash;
use zaino_internal_value_balance as value_balance;
use zaino_persistence::{IndexKind, Schema};
use zcash_protocol::consensus::NetworkType;

pub(crate) fn schema(kind: IndexKind, network: NetworkType) -> Schema {
    let (format, tables) = match kind {
        IndexKind::CompactBlock => (compact_block::FORMAT, compact_block::TABLES),
        IndexKind::ValueBalance => (value_balance::FORMAT, value_balance::TABLES),
        IndexKind::BlockHash => (block_hash::FORMAT, block_hash::TABLES),
        IndexKind::TreeState => (tree_state::FORMAT, tree_state::TABLES),
        IndexKind::TransparentAddress => (transparent_address::FORMAT, transparent_address::TABLES),
        IndexKind::HeaderChain => (zaino_header_chain::FORMAT, zaino_header_chain::TABLES),
    };
    Schema::new(kind, format, network, tables)
}
