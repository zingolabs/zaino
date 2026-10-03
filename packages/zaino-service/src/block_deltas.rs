//! The `getblockdeltas` composed read — a block's transparent value movements.
//!
//! `getblockdeltas` is an indexer-only method: Zebra answers `-32601`, so the
//! shape authority is zcashd's `blockToDeltasJSON` (`rpc/blockchain.cpp`). It is
//! **composed**, never a new source port: the engine assembles it from the reads
//! it already serves — the block header ([`BlockRead`](crate::BlockRead)), its
//! chain position ([`BlockVerboseRead`](crate::BlockVerboseRead)), its resolved
//! transactions ([`TransactionViewRead`](crate::TransactionViewRead)), and the
//! predecessor header times ([`HeaderRead`](crate::HeaderRead)) its median time
//! is taken over.
//!
//! The domain shape carries the facts; it does not render them. Each transparent
//! movement keeps the spent/created output's [`Script`] rather than an encoded
//! address, because encoding an address needs the network — a serving parameter
//! the adapter holds, not a capability the engine has. The adapter decodes the
//! script to a P2PKH/P2SH address exactly as zcashd does, omitting the address
//! for any other script.

use std::future::Future;

use zaino_primitives::types::{
    AbsoluteChainWork, BlockHash, BlockSelector, BlockTime, CompactDifficulty, Confirmations,
    Difficulty, EquihashNonce, Height, MerkleRoot, OutputIndex, Script, SignedZatoshis,
    TransactionId, TxIndex, Zatoshis,
};

use crate::error::BlockDeltasError;

/// One transparent input of a transaction, as a negative value movement.
///
/// Mirrors a zcashd `inputs[]` entry: the spend removes value from a transparent
/// address, so [`satoshis`](Self::satoshis) is **negative** — the negation of
/// the spent output's value. The address is carried as the spent output's
/// [`script`](Self::script); the adapter decodes it to a P2PKH/P2SH address, or
/// omits the address for any other script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputDelta {
    /// The spent output's locking script — the address the value left.
    pub script: Script,
    /// The negation of the spent output's value: a spend is a negative delta.
    pub satoshis: SignedZatoshis,
    /// This input's index within the spending transaction (`vin` index).
    pub index: u32,
    /// The transaction whose output this input spends.
    pub prev_txid: TransactionId,
    /// The index of the spent output within its transaction.
    pub prevout: OutputIndex,
}

/// One transparent output of a transaction, as a positive value movement.
///
/// Mirrors a zcashd `outputs[]` entry: the output adds value to a transparent
/// address, so [`satoshis`](Self::satoshis) is the output's (positive) value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputDelta {
    /// The output's locking script — the address the value arrived at.
    pub script: Script,
    /// The output's value: a receive is a positive delta.
    pub satoshis: Zatoshis,
    /// This output's index within the transaction (`vout` index).
    pub index: OutputIndex,
}

/// One transaction's transparent value movements.
///
/// A coinbase has no [`inputs`](Self::inputs) — zcashd skips the inputs of a
/// coinbase — so the vector is empty for the block's first transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransactionDeltas {
    /// The transaction's id.
    pub txid: TransactionId,
    /// The transaction's offset within the block (0-based).
    pub index: TxIndex,
    /// The transparent inputs, in `vin` order. Empty for a coinbase.
    pub inputs: Vec<InputDelta>,
    /// The transparent outputs, in `vout` order.
    pub outputs: Vec<OutputDelta>,
}

/// A block's transparent value movements and the chain-position header fields
/// zcashd's `getblockdeltas` reports alongside them.
///
/// The header fields mirror `blockToDeltasJSON`: the block's own
/// (`hash`, `size`, `height`, `version`, `merkleroot`, `time`, `nonce`, `bits`)
/// and its chain-position facts (`confirmations`, `mediantime`, `difficulty`,
/// `chainwork`, `previousblockhash`, `nextblockhash`). [`chainwork`](Self::chainwork)
/// is `None` from a validator that does not track cumulative work, matching
/// `getblock`. [`prev_hash`](Self::prev_hash) is `None` for genesis;
/// [`next_hash`](Self::next_hash) is `None` at the tip.
#[derive(Debug, Clone, PartialEq)]
pub struct BlockDeltas {
    /// The block's hash.
    pub hash: BlockHash,
    /// Depth of this block in the best chain.
    pub confirmations: Confirmations,
    /// Serialized byte length of the whole block.
    pub size: u64,
    /// The block's height.
    pub height: Height,
    /// The block's version.
    pub version: u32,
    /// The block's transaction merkle root.
    pub merkle_root: MerkleRoot,
    /// The per-transaction transparent deltas, in block order.
    pub deltas: Vec<TransactionDeltas>,
    /// The block's timestamp (`nTime`).
    pub time: BlockTime,
    /// The median of the block's own time and its ten predecessors' times
    /// (`GetMedianTimePast`), taken over local header reads.
    pub median_time: BlockTime,
    /// The Equihash nonce.
    pub nonce: EquihashNonce,
    /// The compact difficulty target (`nBits`).
    pub bits: CompactDifficulty,
    /// Difficulty at this block, as a multiple of the network minimum.
    pub difficulty: Difficulty,
    /// Cumulative chainwork at this block, or `None` when the validator does not
    /// track it.
    pub chainwork: Option<AbsoluteChainWork>,
    /// The previous block's hash, or `None` for genesis.
    pub prev_hash: Option<BlockHash>,
    /// The next block's hash on the best chain, or `None` at the tip.
    pub next_hash: Option<BlockHash>,
}

/// The composed `getblockdeltas` read: a block's transparent value movements,
/// assembled from the reads the engine already serves.
///
/// A miss on the requested block is a domain answer (`Ok(None)`); the error
/// variants are the resolution, chain-view and header-hole inconsistencies that
/// can arise while composing a block the reads *did* serve. It adds no source
/// port: every input is a read the node-RPC set already holds.
pub trait BlockDeltasRead: Send + Sync {
    /// The transparent value movements of the block `at`, or `Ok(None)` when the
    /// selector names no block.
    fn block_deltas(
        &self,
        at: BlockSelector,
    ) -> impl Future<Output = Result<Option<BlockDeltas>, BlockDeltasError>> + Send;
}
