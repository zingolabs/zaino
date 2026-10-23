//! Wire-boundary conversions between business-layer types and the gRPC
//! proto types defined in `zaino-proto`.
//!
//! Named functions rather than `From`, per the wire-boundary rule, and free
//! functions because the types they convert live in other crates (the storage
//! backend, `zaino-primitives`).

use super::types::BlockIndex;
use zaino_primitives::types::rpc::StandardFee;
use zaino_proto::proto::service::{self, BlockId};

/// Build a wire-format `BlockId` from a business-layer `BlockIndex`.
pub fn block_index_to_wire(index: &BlockIndex) -> BlockId {
    BlockId {
        height: u64::from(index.height.0),
        hash: index.hash.0.to_vec(),
    }
}

/// Build the wire-format `GetStandardFee` reply from the validator's answer.
pub(crate) fn standard_fee_to_wire(fee: &StandardFee) -> service::StandardFee {
    service::StandardFee {
        fee_zat: fee.fee_per_action.as_u64(),
        estimator_version: fee.version,
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

    /// Field-level golden: `fee_per_action` → `feeZat`, `version` → `estimatorVersion`.
    #[test]
    fn standard_fee_to_wire_golden() {
        let fee = StandardFee {
            fee_per_action: zaino_primitives::types::Zatoshis::new(5_000)
                .expect("5000 zat is within the supply bound"),
            version: 7,
        };
        let wire = standard_fee_to_wire(&fee);
        assert_eq!(wire.fee_zat, 5_000);
        assert_eq!(wire.estimator_version, 7);
    }
}
