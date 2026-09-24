//! Wire-boundary conversions between business-layer types and the gRPC
//! proto types defined in `zaino-proto`.
//!
//! All conversions at this boundary are named functions rather than
//! `From` / `TryFrom`, for the same reason the storage boundary uses named
//! methods: naming the conversion puts its direction and its boundary in the
//! API surface instead of behind a generic trait.
//!
//! Free functions rather than inherent methods, because the types they
//! convert now live in the storage backend and a crate cannot add inherent
//! methods to a foreign type. That the compiler stops the conversion living
//! next to the type is the boundary working: a protocol shape and a stored
//! shape should not be reachable from one another.

use super::types::BlockIndex;
use zaino_proto::proto::service::BlockId;

/// Build a wire-format `BlockId` from a business-layer `BlockIndex`.
///
/// Infallible: `Height(u32)` widens cleanly to `u64`, and the 32-byte
/// `BlockHash` array copies into a `Vec<u8>`.
///
/// Replaces the manual `BlockId { height: tip.height.0 as u64, hash:
/// tip.hash.0.to_vec() }` pattern at gRPC egress points.
pub fn block_index_to_wire(index: &BlockIndex) -> BlockId {
    BlockId {
        height: u64::from(index.height.0),
        hash: index.hash.0.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    //! Tests for the `BlockIndex` → wire boundary.
    //!
    //! The `to_wire` golden pins the field-level mapping — any structural
    //! drift in `BlockIndex` or `proto::BlockId` that would change the
    //! on-the-wire bytes of `CompactTxStreamer` responses fails this test.

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
