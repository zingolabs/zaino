//! Responses that are a single hash: `getbestblockhash`, `sendrawtransaction`.
//!
//! Both render as one hex string in RPC display order, and both reuse Zebra's
//! own type — there is nothing zcashd-specific left to reimplement, so this
//! module holds only the conversions.

use zaino_primitives::types::{BlockHash, TransactionId};
use zebra_rpc::methods::{GetBlockHash, SentTransactionHash};

/// Renders a block hash as the `getbestblockhash` response.
pub fn best_block_hash_from_domain(hash: BlockHash) -> GetBlockHash {
    GetBlockHash::new(zebra_chain::block::Hash(hash.into()))
}

/// Renders a transaction hash as the `sendrawtransaction` response.
pub fn sent_transaction_hash_from_domain(txid: TransactionId) -> SentTransactionHash {
    SentTransactionHash::new(zebra_chain::transaction::Hash::from(<[u8; 32]>::from(txid)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asymmetric under reversal, so a missing or doubled byte-reversal shows up.
    const ASYMMETRIC: [u8; 32] = [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        0xff, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
        0xee, 0x01,
    ];

    fn display_order() -> String {
        let mut bytes = ASYMMETRIC;
        bytes.reverse();
        hex::encode(bytes)
    }

    /// Both responses are a bare JSON string in display order. The domain types
    /// hold internal order, so a missing reversal here would name a different
    /// block or transaction while still looking like valid hex.
    #[test]
    fn both_render_as_display_order_hex() {
        assert_eq!(
            serde_json::to_value(best_block_hash_from_domain(BlockHash::from(ASYMMETRIC))).unwrap(),
            serde_json::Value::String(display_order())
        );
        assert_eq!(
            serde_json::to_value(sent_transaction_hash_from_domain(TransactionId::from(
                ASYMMETRIC
            )))
            .unwrap(),
            serde_json::Value::String(display_order())
        );
    }
}
