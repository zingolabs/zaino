//! Types associated with the `getblockdeltas` RPC request.

use zebra_chain::amount::{Amount, NonNegative};

use crate::jsonrpsee::connector::{ResponseToError, RpcError};

/// Error type for the `getblockdeltas` RPC request.
#[derive(Debug, thiserror::Error)]
pub enum BlockDeltasError {
    /// Block not found.
    #[error("Block not found: {0}")]
    BlockNotFound(String),

    /// Error while calculating median time past
    #[error("Error while calculating median time past")]
    CalculationError,

    /// Received a raw block when expecting a block object
    #[error("Received a raw block when expecting a block object")]
    UnexpectedRawBlock,
}

/// Response to a `getblockdeltas` RPC request.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct BlockDeltas {
    /// The hash of the block.
    pub hash: String,

    /// The number of confirmations.
    pub confirmations: i64,

    /// Serialized block size in bytes.
    pub size: i64,

    /// Block height in the best chain.
    pub height: u32,

    /// Block header version.
    pub version: u32,

    /// The merkle root of the block.
    #[serde(rename = "merkleroot")]
    pub merkle_root: String,

    /// Per-transaction transparent deltas for this block.
    /// Each entry corresponds to a transaction at position `index` in the block and
    /// contains:
    /// - `inputs`: non-coinbase vins with **negative** zatoshi amounts and their prevouts,
    /// - `outputs`: vouts with exactly one transparent address and **positive** amounts.
    pub deltas: Vec<BlockDelta>,

    /// Block header timestamp as set by the miner.
    pub time: i64,

    /// Median-Time-Past (MTP) of this block, i.e. the median of the timestamps of
    /// this block and up to the 10 previous blocks `[N-10 … N]` (Unix epoch seconds).
    #[serde(rename = "mediantime")]
    pub median_time: i64,

    /// Block header nonce encoded as hex (Equihash nonce).
    pub nonce: String,

    /// Compact target (“nBits”) as a hex string, e.g. `"1d00ffff"`.
    pub bits: String,

    /// Difficulty corresponding to `bits` (relative to minimum difficulty, e.g. `1.0`).
    pub difficulty: f64,

    // `chainwork` would be here, but Zebra does not plan to support it
    // pub chainwork: Vec<u8>,
    /// Previous block hash as hex, or `None` for genesis.
    #[serde(skip_serializing_if = "Option::is_none", rename = "previousblockhash")]
    pub previous_block_hash: Option<String>,

    /// Next block hash in the active chain, if known. Omitted for the current tip
    /// or for blocks not in the active chain.
    #[serde(skip_serializing_if = "Option::is_none", rename = "nextblockhash")]
    pub next_block_hash: Option<String>,
}

impl ResponseToError for BlockDeltas {
    type RpcError = BlockDeltasError;
}

impl TryFrom<RpcError> for BlockDeltasError {
    type Error = RpcError;

    fn try_from(value: RpcError) -> Result<Self, Self::Error> {
        if value.code == -8 {
            Ok(Self::UnexpectedRawBlock)
        } else {
            Err(value)
        }
    }
}

/// Per-transaction transparent deltas within a block, as returned by
/// `getblockdeltas`. One `BlockDelta` is emitted for each transaction in
/// the block, at the transaction’s position (`index`).
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq)]
pub struct BlockDelta {
    /// Transaction hash.
    pub txid: String,

    /// Zero-based position of this transaction within the block.
    pub index: u32,

    /// Transparent input deltas (non-coinbase only).
    ///
    /// Each entry spends a previous transparent output and records a **negative**
    /// amount in zatoshis. Inputs that do not resolve to exactly one transparent
    /// address are omitted.
    pub inputs: Vec<InputDelta>,

    /// Transparent output deltas.
    ///
    /// Each entry pays exactly one transparent address and records a **positive**
    /// amount in zatoshis. Outputs without a single transparent address (e.g.,
    /// OP_RETURN, bare multisig with multiple addresses) are omitted.
    pub outputs: Vec<OutputDelta>,
}

/// A single transparent input delta within a transaction.
///
/// Represents spending of a specific previous output (`prevtxid`/`prevout`)
/// to a known transparent address. Amounts are **negative** (funds leaving).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct InputDelta {
    /// Transparent address that the spent prevout paid to.
    pub address: String,

    /// Amount in zatoshis, **negative** for inputs/spends.
    pub satoshis: Amount,

    /// Zero-based vin index within the transaction.
    pub index: u32,

    /// Hash of the previous transaction containing the spent output.
    pub prevtxid: String,

    /// Output index (`vout`) in `prevtxid` that is being spent.
    pub prevout: u32,
}

/// A single transparent output delta within a transaction.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct OutputDelta {
    /// Transparent address paid by this output.
    pub address: String,

    /// Amount in zatoshis, **non-negative**.
    pub satoshis: Amount<NonNegative>,

    /// Zero-based vout index within the transaction.
    pub index: u32,
}
