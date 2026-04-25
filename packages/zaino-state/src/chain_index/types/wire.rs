//! Wire-boundary conversions between business-layer types and the gRPC
//! proto types defined in `zaino-proto`.
//!
//! All conversions at this boundary use named inherent methods instead of
//! `From` / `TryFrom`, for the same reasons the DB boundary does — the
//! wire → business direction *is* the external-input validation step, and
//! the named method encodes that contract in the API surface. See
//! `CLAUDE.md` §"Persistence-boundary conversions" for the project rule;
//! this module applies the same rule to wire conversions.

use super::{BlockHash, BlockIndex, Height};
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

    /// Build a `BlockIndex` from a wire-format `BlockId`, validating that
    /// the wire payload fits the narrower business-layer constraints.
    ///
    /// This conversion **is** the wire-input validation step. The two
    /// narrowings checked:
    ///   1. `BlockId.hash: Vec<u8>` must be exactly 32 bytes.
    ///   2. `BlockId.height: u64` must fit in `u32`.
    ///
    /// Replaces `impl TryFrom<proto::BlockId> for BlockIndex`. The named
    /// method plus the typed [`WireBlockIdError`] puts the validation
    /// contract in the API surface rather than behind a generic trait.
    pub fn try_from_wire(wire: BlockId) -> Result<Self, WireBlockIdError> {
        let hash_len = wire.hash.len();
        let hash_array: [u8; 32] = wire
            .hash
            .try_into()
            .map_err(|_| WireBlockIdError::HashWrongLength { got: hash_len })?;
        let height_u32 = u32::try_from(wire.height)
            .map_err(|_| WireBlockIdError::HeightOverflow { got: wire.height })?;
        Ok(Self {
            height: Height(height_u32),
            hash: BlockHash(hash_array),
        })
    }
}

/// Ways in which `BlockIndex::try_from_wire` can reject its input.
///
/// Each variant documents one class of wire-payload shape that cannot
/// be represented by the business-layer [`BlockIndex`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireBlockIdError {
    /// The wire `BlockId.hash` bytestring was not exactly 32 bytes long.
    HashWrongLength {
        /// The length the wire produced.
        got: usize,
    },
    /// The wire `BlockId.height` value did not fit in `u32`.
    HeightOverflow {
        /// The u64 height the wire produced.
        got: u64,
    },
}

impl std::fmt::Display for WireBlockIdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HashWrongLength { got } => {
                write!(f, "wire BlockId.hash has {got} bytes; expected 32")
            }
            Self::HeightOverflow { got } => {
                write!(f, "wire BlockId.height = {got} does not fit in u32")
            }
        }
    }
}

impl std::error::Error for WireBlockIdError {}

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

    /// Narrow wire round-trip: `BlockIndex → proto::BlockId → BlockIndex`
    /// is identity for a canonical value.
    ///
    /// Paired with the cross-boundary test in `types/db/block.rs`: if this
    /// narrow test passes but the cross-boundary one fails, the bug is on
    /// the DB side or at the DB↔business crossing, not in the wire
    /// conversion itself.
    #[test]
    fn block_index_round_trips_through_wire() {
        let idx = BlockIndex {
            height: Height(0x0dec_0de0),
            hash: BlockHash::from([0x11u8; 32]),
        };
        let wire = idx.to_wire();
        let recovered = BlockIndex::try_from_wire(wire).expect("valid wire shape");
        assert_eq!(idx, recovered);
    }

    /// Rejection: a wire hash shorter than 32 bytes fails with a precise
    /// `HashWrongLength` error rather than silently truncating / panicking.
    #[test]
    fn try_from_wire_rejects_short_hash() {
        let wire = BlockId {
            height: 1,
            hash: vec![0x00; 31],
        };
        assert_eq!(
            BlockIndex::try_from_wire(wire),
            Err(WireBlockIdError::HashWrongLength { got: 31 })
        );
    }

    /// Rejection: a wire height that overflows `u32` fails with a precise
    /// `HeightOverflow` error.
    #[test]
    fn try_from_wire_rejects_u32_overflow_height() {
        let wire = BlockId {
            height: u64::from(u32::MAX) + 1,
            hash: vec![0x11; 32],
        };
        assert_eq!(
            BlockIndex::try_from_wire(wire),
            Err(WireBlockIdError::HeightOverflow {
                got: u64::from(u32::MAX) + 1
            })
        );
    }
}
