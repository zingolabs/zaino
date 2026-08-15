//! Transaction and per-pool data.

use super::{
    EncryptedCiphertext, EphemeralKey, NoteCommitment, Nullifier, OutputIndex, Script,
    SignedZatoshis, TransactionId, TxIndex, Zatoshis,
};

/// A transaction within a block.
#[derive(Debug, Clone)]
pub struct Transaction {
    /// Transaction id.
    ///
    /// NOTE: Transaction hash vs transaction ID
    /// - In pre V5 transactions this is the transaction hash (sha256 of serialized tx).
    /// - From V5 onwards this field is the transaction ID (as defined in [zip 224](https://github.com/zcash/zips/blob/main/zips/zip-0244.rst).
    pub txid: TransactionId,
    /// Position within the block (0-indexed).
    pub index: TxIndex,
    /// Transparent pool data.
    pub transparent: TransparentData,
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

/// Transparent pool data within a transaction.
#[derive(Debug, Clone, Default)]
pub struct TransparentData {
    /// Transparent inputs (spent outpoints).
    pub inputs: Vec<TransparentInput>,
    /// Transparent outputs.
    pub outputs: Vec<TransparentOutput>,
}

/// A transparent input: reference to a previous output being spent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransparentInput {
    /// Transaction containing the output being spent.
    pub prev_txid: TransactionId,
    /// Index of the output being spent.
    pub prev_index: OutputIndex,
}

/// A transparent output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransparentOutput {
    /// Value in zatoshis.
    pub value: Zatoshis,
    /// Output script.
    pub script: Script,
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
    /// Partial encrypted ciphertext (52 bytes, enough for scanning).
    pub enc_ciphertext: EncryptedCiphertext,
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
    /// Partial encrypted ciphertext (52 bytes, enough for scanning).
    pub enc_ciphertext: EncryptedCiphertext,
}
