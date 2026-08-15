//! `gettxout` — an unspent transparent output.

use crate::types::{BlockHash, Confirmations, Script, TransparentAddress, Zatoshis};

/// An unspent transparent output, as reported by `gettxout`.
///
/// The query answers "is this outpoint unspent, and if so what is in it?", so
/// a spent or unknown outpoint is reported as no result at all rather than as
/// a variant here.
///
/// This type is newly modelled rather than ported: the previous wire form was
/// an untyped `Option<serde_json::Value>`, so every field below is derived from
/// the Zcash JSON-RPC interface rather than from an existing Rust shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxOut {
    /// Best-chain tip the validator answered against.
    ///
    /// Pairs with [`Self::confirmations`]: both are relative to this tip, so a
    /// consumer can tell whether two answers were computed at the same height.
    pub best_block: BlockHash,

    /// Depth of the containing block, or `0` when the output is in the mempool.
    pub confirmations: Confirmations,

    /// Value held by the output.
    ///
    /// The wire form is ZEC-denominated; the adapter converts to integer
    /// zatoshis.
    pub value: Zatoshis,

    /// The output's locking script.
    pub script_pub_key: ScriptPubKey,

    /// Whether the output was created by a coinbase transaction.
    ///
    /// Coinbase outputs are subject to a maturity delay, so a consumer cannot
    /// treat an unspent coinbase output as spendable on confirmations alone.
    pub coinbase: bool,
}

/// A locking script, with the interpretation the validator derived from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptPubKey {
    /// The raw script.
    pub script: Script,

    /// Human-readable disassembly of the script.
    ///
    /// `None` from validators that do not disassemble. Purely descriptive —
    /// it is derived from [`Self::script`], never authoritative over it.
    pub asm: Option<String>,

    /// The validator's classification of the script, e.g. `"pubkeyhash"`.
    ///
    /// `None` when the validator does not classify, and expected to be absent
    /// or unrecognised for scripts that match no standard template.
    pub script_type: Option<String>,

    /// Signatures required to spend, when the validator reports it.
    ///
    /// `None` for script forms where the count is not well defined.
    pub required_signatures: Option<u32>,

    /// Addresses the validator attributed the output to.
    ///
    /// Empty when it attributed none — which is the normal case for scripts
    /// that do not correspond to an address. Multi-address attribution is
    /// possible for bare multisig.
    pub addresses: Vec<TransparentAddress>,
}
