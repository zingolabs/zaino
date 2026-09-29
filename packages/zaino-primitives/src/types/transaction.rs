//! Transaction and per-pool data.

use super::{
    CompactCiphertext, EphemeralKey, NoteCommitment, Nullifier, OutputIndex, Script,
    SignedZatoshis, TransactionId, Zatoshis,
};

/// A transaction within a block.
///
/// A transaction carries no position field. Its slot in the block — and so
/// whether it is the coinbase (position 0) — is a property of the
/// [`Block`](super::Block) that holds it, read from the order of
/// [`Block::transactions`](super::Block::transactions), never restated here.
/// A `Transaction` outside a block (a mempool transaction) has no position at
/// all, so there is no value to invent for one.
#[derive(Debug, Clone)]
pub struct Transaction {
    /// Transaction id.
    ///
    /// NOTE: Transaction hash vs transaction ID
    /// - In pre V5 transactions this is the transaction hash (sha256 of serialized tx).
    /// - From V5 onwards this field is the transaction ID (as defined in [zip 224](https://github.com/zcash/zips/blob/main/zips/zip-0244.rst).
    pub txid: TransactionId,
    /// Transparent pool data.
    pub transparent: TransparentData,
    pub sprout: SproutData,
    /// Sapling pool data.
    pub sapling: SaplingData,
    /// Orchard pool data.
    pub orchard: OrchardData,
    /// Ironwood pool data (NU6.3).
    ///
    /// Ironwood actions are structurally identical to Orchard actions, so the
    /// pool reuses [`OrchardData`] rather than duplicating the shape. It is a
    /// separate field, not merged into `orchard`: the two pools have separate
    /// commitment trees, separate value balances, and are independently
    /// selectable by the compact-block pool filter.
    pub ironwood: OrchardData,
}

impl Transaction {
    /// Heap bytes held (allocation capacities; the inline struct excluded)
    pub(crate) fn heap_size(&self) -> usize {
        fn slots<T>(items: &Vec<T>) -> usize {
            size_of::<T>() * items.capacity()
        }
        let transparent = &self.transparent;
        let scripts: usize =
            transparent.outputs.iter().map(|output| output.script.heap_size()).sum();
        slots(&transparent.inputs)
            + slots(&transparent.outputs)
            + scripts
            + slots(&self.sapling.spends)
            + slots(&self.sapling.outputs)
            + slots(&self.orchard.actions)
            + slots(&self.ironwood.actions)
    }
}

/// Transparent pool data within a transaction.
#[derive(Debug, Clone, Default)]
pub struct TransparentData {
    /// Transparent inputs, each = the outpoint it spends (coinbase inputs elided at decode)
    pub inputs: Vec<OutPoint>,
    /// Transparent outputs.
    pub outputs: Vec<TransparentOutput>,
}

/// One transparent output, named by its transaction and position
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OutPoint {
    pub txid: TransactionId,
    pub vout: OutputIndex,
}

/// A transparent output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransparentOutput {
    /// Value in zatoshis.
    pub value: Zatoshis,
    /// Output script.
    pub script: Script,
}

/// Sprout pool data within a transaction (value only: no compact-format fields)
#[derive(Debug, Clone, Default)]
pub struct SproutData {
    /// Σ `vpub_new` − Σ `vpub_old` over the JoinSplits (positive = value flows out of the pool)
    pub value_balance: SignedZatoshis,
}

/// Sapling pool data within a transaction.
#[derive(Debug, Clone, Default)]
pub struct SaplingData {
    /// Sapling spends (nullifiers).
    pub spends: Vec<SaplingSpend>,
    /// Sapling outputs.
    pub outputs: Vec<SaplingOutput>,
    /// Net value balance (positive = value flows out of the pool).
    pub value_balance: SignedZatoshis,
}

/// A Sapling spend: the nullifier that marks a note as consumed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaplingSpend {
    /// Nullifier.
    pub nullifier: Nullifier,
}

/// A Sapling output: commitment + detection material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaplingOutput {
    /// Note commitment (cmu).
    pub cmu: NoteCommitment,
    /// Ephemeral key for recipient detection.
    pub ephemeral_key: EphemeralKey,
    /// Compact ciphertext head (52 bytes, enough for scanning).
    pub enc_ciphertext: CompactCiphertext,
}

/// Orchard pool data within a transaction.
#[derive(Debug, Clone, Default)]
pub struct OrchardData {
    /// Orchard actions (each is both a spend and an output).
    pub actions: Vec<OrchardAction>,
    /// Net value balance (positive = value flows out of the pool).
    pub value_balance: SignedZatoshis,
}

/// An Orchard action: nullifier + commitment + detection material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrchardAction {
    /// Nullifier.
    pub nullifier: Nullifier,
    /// Note commitment (cmx).
    pub cmx: NoteCommitment,
    /// Ephemeral key for recipient detection.
    pub ephemeral_key: EphemeralKey,
    /// Compact ciphertext head (52 bytes, enough for scanning).
    pub enc_ciphertext: CompactCiphertext,
}
