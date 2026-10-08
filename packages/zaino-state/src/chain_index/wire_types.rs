//! Wire-boundary conversions between business-layer types and the gRPC
//! proto types defined in `zaino-proto`.
//!
//! Named functions rather than `From`, per the wire-boundary rule, and free
//! functions because the types they convert live in the storage backend.

use super::types::BlockIndex;
use zaino_proto::proto::service::BlockId;

/// Build a wire-format `BlockId` from a business-layer `BlockIndex`.
pub fn block_index_to_wire(index: &BlockIndex) -> BlockId {
    BlockId {
        height: u64::from(index.height.0),
        hash: index.hash.0.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain_index::types::{BlockHash, Height};

    /// Field-level golden: a canonical `BlockIndex` maps to a precise
    /// `(height: u64, hash: Vec<u8>)` wire pair.
    #[test]
    fn block_index_to_wire_block_id_golden() {
        let idx = BlockIndex {
            height: Height(0x0dec_0de0),
            hash: BlockHash::from([0x11u8; 32]),
        };
        let wire = block_index_to_wire(&idx);
        assert_eq!(wire.height, 0x0dec_0de0_u64);
        assert_eq!(wire.hash, vec![0x11u8; 32]);
    }
}
