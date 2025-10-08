//! Types associated with the `gettxoutsetinfo` RPC request.
//!
//! Although the current threat model assumes that `zaino` connects to a trusted validator,
//! the `gettxoutsetinfo` RPC performs some light validation.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::jsonrpsee::response::common::{amount::ZecAmount, block::BlockHash, BlockHeight};

/// Response to a `gettxoutsetinfo` RPC request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum GetTxOutSetInfo {
    /// Validated payload
    Known(TxOutSetInfo),

    /// Unrecognized shape
    Unknown(Value),
}

/// Response to a `gettxoutsetinfo` RPC request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TxOutSetInfo {
    /// The current block height (index).
    pub height: BlockHeight,

    /// The best block hash hex.
    #[serde(rename = "bestblock")]
    pub best_block: BlockHash,

    /// The number of transactions.
    pub transactions: u64,

    /// The number of output transactions.
    #[serde(rename = "txouts")]
    pub tx_outs: u64,

    /// The serialized size
    pub bytes_serialized: u64,

    /// The serialized hash
    pub hash_serialized: String,

    /// The total amount
    pub total_amount: ZecAmount,
}
