//! Pre-index and serving compact block types.
//!
//! A [`PreIndexCompactBlock`] is what the source adapter provides: block data
//! with proofs/sigs stripped, suitable for feeding the sync engine. It does NOT
//! contain indexed state like commitment tree sizes.
//!
//! A [`CompactBlock`] is the serving-ready format (proto-adherent) that includes
//! [`ChainMetadata`]. It's the OUTPUT of indexing — constructed by reading from
//! the indexed DB, not from the source directly.
//!
//! Transaction sub-types (`TransparentInput`, `SaplingOutput`, `OrchardAction`,
//! etc.) are reused from [`super::transaction`] — no duplication.

use super::{
    BlockHash, BlockTime, ChainMetadata, CompactDifficulty, Nullifier, OrchardAction,
    SaplingOutput, TransactionHash, TransparentInput, TransparentOutput,
};

// ---------------------------------------------------------------------------
// Pre-index compact block — source adapter output
// ---------------------------------------------------------------------------

/// A compact block as received from the source, before indexing.
///
/// Contains all per-block data needed to build indexes, but no cumulative
/// indexed state (tree sizes, etc.). The sync engine consumes this type.
///
/// Becomes a [`CompactBlock`] after indexing adds [`ChainMetadata`].
#[derive(Debug, Clone)]
pub struct PreIndexCompactBlock {
    /// Block hash.
    pub hash: BlockHash,
    /// Previous block hash.
    pub prev_hash: BlockHash,
    /// Block height (raw u32).
    pub height: u32,
    /// Block timestamp.
    pub time: BlockTime,
    /// Compact difficulty target (nBits).
    pub bits: CompactDifficulty,
    /// Compact transactions.
    pub transactions: Vec<PreIndexCompactTx>,
}

/// A compact transaction in the pre-index representation.
///
/// Reuses existing primitives types for sub-components.
#[derive(Debug, Clone)]
pub struct PreIndexCompactTx {
    /// Transaction hash.
    pub txid: TransactionHash,
    /// Transparent inputs — outpoint only (no script/sequence).
    pub transparent_inputs: Vec<TransparentInput>,
    /// Transparent outputs — value + lock script.
    pub transparent_outputs: Vec<TransparentOutput>,
    /// Sapling nullifiers (one per spend).
    pub sapling_nullifiers: Vec<Nullifier>,
    /// Sapling outputs — cmu + epk + enc_ciphertext head.
    pub sapling_outputs: Vec<SaplingOutput>,
    /// Orchard actions — nullifier + cmx + epk + enc_ciphertext head.
    pub orchard_actions: Vec<OrchardAction>,
}

impl From<&super::Block> for PreIndexCompactBlock {
    fn from(block: &super::Block) -> Self {
        Self {
            hash: block.header.hash,
            prev_hash: block.header.prev_hash,
            height: u32::from(block.header.height),
            time: block.header.time,
            bits: block.header.bits,
            transactions: block.transactions.iter().map(PreIndexCompactTx::from).collect(),
        }
    }
}

impl From<&super::transaction::Transaction> for PreIndexCompactTx {
    fn from(tx: &super::transaction::Transaction) -> Self {
        Self {
            txid: tx.txid,
            transparent_inputs: tx.transparent.inputs.clone(),
            transparent_outputs: tx.transparent.outputs.clone(),
            sapling_nullifiers: tx.sapling.spends.iter().map(|s| s.nullifier).collect(),
            sapling_outputs: tx.sapling.outputs.clone(),
            orchard_actions: tx.orchard.actions.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// CompactBlock — serving format (proto-adherent, post-indexing)
// ---------------------------------------------------------------------------

/// A complete compact block ready for serving to light wallets.
///
/// This is the proto-adherent `CompactBlock` format. It includes
/// [`ChainMetadata`] (commitment tree sizes) which is cumulative indexed
/// state — not available from the raw block data.
///
/// Constructed by reading from the indexed DB after sync completes.
#[derive(Debug, Clone)]
pub struct CompactBlock {
    /// Block hash.
    pub hash: BlockHash,
    /// Previous block hash.
    pub prev_hash: BlockHash,
    /// Block height (raw u32).
    pub height: u32,
    /// Block timestamp.
    pub time: BlockTime,
    /// Compact difficulty target (nBits).
    pub bits: CompactDifficulty,
    /// Compact transactions (same shape as pre-index).
    pub transactions: Vec<PreIndexCompactTx>,
    /// Indexed chain metadata — commitment tree sizes at this block.
    pub chain_metadata: ChainMetadata,
}
