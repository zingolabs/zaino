//! Whether the pinned view considers a transparent output spent.

use zaino_primitives::types::TransactionHash;

/// Whether the pinned view considers a transparent output spent.
///
/// Spentness is authoritative — it comes from the engine's UTXO set —
/// while resolving the spending transaction may require a per-outpoint
/// spend index the engine does not maintain. An engine that knows the
/// output is spent but cannot name the spender answers
/// [`SpendStatus::SpentSpenderUnknown`], so the caller retries rather
/// than concluding the output is unspent
/// (ZcashFoundation/zebra#10806).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpendStatus {
    /// The output is unspent as of the pinned tip.
    Unspent,
    /// The output was spent by this transaction.
    SpentBy(TransactionHash),
    /// The output is spent, but the engine cannot resolve the spending
    /// transaction.
    SpentSpenderUnknown,
}
