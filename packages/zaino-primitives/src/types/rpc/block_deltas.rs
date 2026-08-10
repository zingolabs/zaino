//! `getblockdeltas` — per-transaction transparent value movements in a block.

use crate::types::{
    BlockHash, BlockTime, CompactDifficulty, Confirmations, Difficulty, EquihashNonce, Height,
    MerkleRoot, OutputIndex, SignedZatoshis, TransactionId, TransparentAddress, TxIndex, Zatoshis,
};

/// Transparent value movements for every transaction in a block.
#[derive(Debug, Clone, PartialEq)]
pub struct BlockDeltas {
    /// Hash of this block.
    pub hash: BlockHash,
    /// Depth of this block in the best chain, or `-1` if it is not on it.
    pub confirmations: Confirmations,
    /// Serialised size of the block, in bytes.
    pub size: u64,
    /// Height of this block.
    pub height: Height,
    /// Header version field.
    pub version: u32,
    /// Merkle root of this block's transaction tree.
    pub merkle_root: MerkleRoot,
    /// One entry per transaction, in block order.
    pub deltas: Vec<BlockDelta>,
    /// Block time as set by the miner, in seconds since the Unix epoch.
    pub time: BlockTime,
    /// Median-time-past: the median timestamp of this block and up to the ten
    /// before it. Monotonic where [`Self::time`] is not, so consensus rules use
    /// it rather than the miner-set value.
    pub median_time: BlockTime,
    /// Header nonce.
    pub nonce: EquihashNonce,
    /// Difficulty threshold in compact (nBits) form.
    pub bits: CompactDifficulty,
    /// Difficulty as a multiple of the network minimum.
    pub difficulty: Difficulty,
    /// Hash of the previous block. `None` for genesis.
    pub previous_block_hash: Option<BlockHash>,
    /// Hash of the next block on the best chain. `None` at the tip, or when
    /// this block is not on the best chain.
    pub next_block_hash: Option<BlockHash>,
}

/// Transparent value movements for one transaction.
///
/// # Deliberately incomplete
///
/// Neither list is a full account of the transaction. The validator omits any
/// input or output it cannot attribute to exactly one transparent address —
/// `OP_RETURN` outputs, bare multisig paying several addresses, and coinbase
/// inputs, which have no previous output. So these deltas do not sum to the
/// transaction's transparent value balance, and must not be used to derive one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockDelta {
    /// Transaction this delta belongs to.
    pub txid: TransactionId,
    /// Position of the transaction within the block.
    pub index: TxIndex,
    /// Attributable transparent inputs. Each removes value from an address.
    pub inputs: Vec<InputDelta>,
    /// Attributable transparent outputs. Each adds value to an address.
    pub outputs: Vec<OutputDelta>,
}

/// One transparent input, spending a previous output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputDelta {
    /// Address the spent output paid to.
    pub address: TransparentAddress,
    /// Value moved. Always negative — this is value leaving the address.
    pub satoshis: SignedZatoshis,
    /// Position of this input within the transaction's inputs.
    pub index: OutputIndex,
    /// Transaction containing the output being spent.
    pub prev_txid: TransactionId,
    /// Index of the output being spent within that transaction.
    pub prev_output: OutputIndex,
}

/// One transparent output, paying an address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputDelta {
    /// Address paid.
    pub address: TransparentAddress,
    /// Value moved. Unsigned because an output can only add value — the wire
    /// form is a non-negative amount, and the type says so.
    pub satoshis: Zatoshis,
    /// Position of this output within the transaction's outputs.
    pub index: OutputIndex,
}
