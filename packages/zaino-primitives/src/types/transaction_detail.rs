//! Transaction facts the indexing [`Transaction`](super::Transaction) drops.
//!
//! The indexing shape keeps only what an index keys on: pools, outpoints,
//! commitments. It deliberately omits the transaction envelope (version,
//! lock time, expiry), the coinbase input (indexing reads it by position, not
//! as data), and the Sprout pool values. The explorer surface needs all three,
//! so they live here, beside the indexing shape rather than inside it.

use super::{Height, Script, Zatoshis};

/// The envelope and the facts the indexing shape omits, for one transaction.
///
/// Every field mirrors what zcashd emits for the same transaction, so that a
/// renderer can map one to one without re-deriving anything from consensus
/// rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransactionDetail {
    /// Transaction version number (`effectiveVersion`): 1 through 6.
    pub version: u32,
    /// Whether the Overwinter format flag is set (version 3 onward).
    pub overwintered: bool,
    /// Version group id, present only on an overwintered transaction.
    pub version_group_id: Option<u32>,
    /// Raw `nLockTime`, exactly as zcashd emits `locktime`.
    pub lock_time: u32,
    /// Expiry height, `None` before Overwinter.
    ///
    /// An overwintered transaction with no expiry carries `Some(Height(0))`,
    /// so that `expiryheight: 0` renders — zcashd's shape — and is kept
    /// distinct from a pre-Overwinter transaction, which has no expiry field at
    /// all.
    pub expiry_height: Option<Height>,
    /// Serialized byte length of the transaction.
    pub size: u64,
    /// The coinbase input, `Some` iff the transaction is a coinbase.
    ///
    /// Coinbase-ness is a fact about the transaction's one input, not about its
    /// position in a block: a coinbase fetched alone is still a coinbase.
    pub coinbase: Option<CoinbaseInput>,
    /// The signature script and sequence of each non-coinbase transparent input,
    /// in the order of
    /// [`Transaction::transparent.inputs`](super::TransparentData::inputs).
    ///
    /// The indexing [`TransparentInput`](super::TransparentInput) keeps only the
    /// spent outpoint, because that is all an index keys on. The explorer's
    /// `vin[].scriptSig` / `vin[].sequence` need the script and sequence too, so
    /// they live here beside the indexing shape — the coinbase precedent, which
    /// keeps its one input's script in [`CoinbaseInput`] rather than widening the
    /// indexing input. Empty on a coinbase transaction, whose input is the
    /// `coinbase` field above.
    pub transparent_inputs: Vec<TransparentInputDetail>,
    /// Sprout pool movements, one per JoinSplit, in transaction order.
    pub joinsplits: Vec<JoinSplitValues>,
}

/// The signature script and sequence of one non-coinbase transparent input.
///
/// Beside the indexing [`TransparentInput`](super::TransparentInput), which keeps
/// only the spent outpoint. These are the facts the explorer's `vin[].scriptSig`
/// and `vin[].sequence` need and an index does not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransparentInputDetail {
    /// The input's signature script (`scriptSig`).
    pub script_sig: Script,
    /// The input's sequence number.
    pub sequence: u32,
}

/// The input of a coinbase transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoinbaseInput {
    /// The coinbase script (`scriptSig`), as zcashd emits its `coinbase` hex.
    pub script: Script,
    /// The input's sequence number.
    pub sequence: u32,
}

/// The transparent value a single Sprout JoinSplit moves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinSplitValues {
    /// Value removed from the transparent pool (`vpub_old`).
    pub vpub_old: Zatoshis,
    /// Value inserted into the transparent pool (`vpub_new`).
    pub vpub_new: Zatoshis,
}
