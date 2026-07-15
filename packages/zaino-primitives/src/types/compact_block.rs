//! Compact block and transaction types for indexers.
//!
//! These mirror the data available from a compact deserialization of Zcash
//! blocks — proofs, signatures, and input scripts are omitted. Sufficient
//! for building all current indexes.

use super::{
    BlockHash, BlockTime, CompactDifficulty, EncryptedCiphertext, EphemeralKey, NoteCommitment,
    Nullifier, OutputIndex, Script, TransactionHash, Zatoshis,
};

impl From<&super::Block> for CompactBlock {
    fn from(block: &super::Block) -> Self {
        Self {
            hash: block.header.hash,
            prev_hash: block.header.prev_hash,
            height: u32::from(block.header.height),
            time: block.header.time,
            bits: block.header.bits,
            transactions: block.transactions.iter().map(CompactTransaction::from).collect(),
        }
    }
}

impl From<&super::transaction::Transaction> for CompactTransaction {
    fn from(tx: &super::transaction::Transaction) -> Self {
        Self {
            txid: tx.txid,
            transparent_inputs: tx
                .transparent
                .inputs
                .iter()
                .map(|inp| CompactTransparentInput {
                    prev_txid: inp.prev_txid,
                    prev_index: inp.prev_index,
                })
                .collect(),
            transparent_outputs: tx
                .transparent
                .outputs
                .iter()
                .map(|out| CompactTransparentOutput {
                    value: out.value,
                    script: out.script.clone(),
                })
                .collect(),
            sapling_nullifiers: tx.sapling.spends.iter().map(|s| s.nullifier).collect(),
            sapling_outputs: tx
                .sapling
                .outputs
                .iter()
                .map(|o| CompactSaplingOutput {
                    cmu: o.cmu,
                    ephemeral_key: o.ephemeral_key,
                    enc_ciphertext: o.enc_ciphertext.clone(),
                })
                .collect(),
            orchard_actions: tx
                .orchard
                .actions
                .iter()
                .map(|a| CompactOrchardAction {
                    nullifier: a.nullifier,
                    cmx: a.cmx,
                    ephemeral_key: a.ephemeral_key,
                    enc_ciphertext: a.enc_ciphertext.clone(),
                })
                .collect(),
        }
    }
}

/// A compact block: header fields + compact transactions.
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
    /// Compact transactions.
    pub transactions: Vec<CompactTransaction>,
}

/// A compact transaction: identity + transparent I/O + shielded compact data.
#[derive(Debug, Clone)]
pub struct CompactTransaction {
    /// Transaction hash.
    pub txid: TransactionHash,
    /// Transparent inputs — outpoint only (no script/sequence).
    pub transparent_inputs: Vec<CompactTransparentInput>,
    /// Transparent outputs — value + lock script.
    pub transparent_outputs: Vec<CompactTransparentOutput>,
    /// Sapling nullifiers (one per spend).
    pub sapling_nullifiers: Vec<Nullifier>,
    /// Sapling outputs — cmu + epk + enc_ciphertext head.
    pub sapling_outputs: Vec<CompactSaplingOutput>,
    /// Orchard actions — nullifier + cmx + epk + enc_ciphertext head.
    pub orchard_actions: Vec<CompactOrchardAction>,
}

/// Compact transparent input: just the outpoint.
#[derive(Debug, Clone)]
pub struct CompactTransparentInput {
    /// Previous transaction hash.
    pub prev_txid: TransactionHash,
    /// Previous output index.
    pub prev_index: OutputIndex,
}

/// Compact transparent output: value + script.
#[derive(Debug, Clone)]
pub struct CompactTransparentOutput {
    /// Value in zatoshis.
    pub value: Zatoshis,
    /// Lock script.
    pub script: Script,
}

/// Compact Sapling output.
#[derive(Debug, Clone)]
pub struct CompactSaplingOutput {
    /// Note commitment (cmu).
    pub cmu: NoteCommitment,
    /// Ephemeral key.
    pub ephemeral_key: EphemeralKey,
    /// First 52 bytes of the encrypted ciphertext.
    pub enc_ciphertext: EncryptedCiphertext,
}

/// Compact Orchard action.
#[derive(Debug, Clone)]
pub struct CompactOrchardAction {
    /// Nullifier.
    pub nullifier: Nullifier,
    /// Note commitment (cmx).
    pub cmx: NoteCommitment,
    /// Ephemeral key.
    pub ephemeral_key: EphemeralKey,
    /// First 52 bytes of the encrypted ciphertext.
    pub enc_ciphertext: EncryptedCiphertext,
}
