//! Each index's store schema + commit `WRITE_BUFFER` (boot opens by it, `verify` scrubs by it)

use std::num::NonZeroUsize;

use zaino_persistence::{IndexKind, Schema};
use zcash_protocol::consensus::NetworkType;

pub(crate) fn schema(kind: IndexKind, network: NetworkType) -> (Schema, NonZeroUsize) {
    let write_buffer = match kind {
        IndexKind::CompactBlock => zaino_index_compact_block::WRITE_BUFFER,
        IndexKind::UtxoSet => zaino_index_utxo_set::WRITE_BUFFER,
        IndexKind::BlockHash => zaino_internal_block_hash_to_height::WRITE_BUFFER,
        IndexKind::TreeState => zaino_index_tree_state::WRITE_BUFFER,
        IndexKind::TransparentAddress => zaino_index_transparent_address::WRITE_BUFFER,
    };
    (zaino_nfs::schema(kind, network), write_buffer)
}
