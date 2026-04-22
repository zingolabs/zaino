//! Wire-boundary conversions between business-layer types and the gRPC
//! proto types defined in `zaino-proto`.
//!
//! All conversions at this boundary use named inherent methods instead of
//! `From` / `TryFrom`, for the same reasons the DB boundary does — the
//! wire → business direction *is* the external-input validation step, and
//! the named method encodes that contract in the API surface. See
//! `CLAUDE.md` §"Persistence-boundary conversions" for the project rule;
//! this module applies the same rule to wire conversions.

use super::BlockIndex;
use zaino_proto::proto::service::BlockId;

impl BlockIndex {
    /// Build a wire-format `BlockId` from this business-layer `BlockIndex`.
    ///
    /// Infallible: `Height(u32)` widens cleanly to `u64`, and the 32-byte
    /// `BlockHash` array copies into a `Vec<u8>`.
    ///
    /// Replaces the manual `BlockId { height: tip.height.0 as u64, hash:
    /// tip.hash.0.to_vec() }` pattern at gRPC egress points.
    pub fn to_wire(&self) -> BlockId {
        BlockId {
            height: u64::from(self.height.0),
            hash: self.hash.0.to_vec(),
        }
    }
}

#[cfg(test)]
mod tests {
    //! Tests for the `BlockIndex` ↔ wire boundary.
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
        let wire = idx.to_wire();
        assert_eq!(wire.height, 0x0dec_0de0_u64);
        assert_eq!(wire.hash, vec![0x11u8; 32]);
    }
}
