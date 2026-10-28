//! A reference to one transparent output.

use super::{OutputIndex, TransactionId};

/// Names one transparent output: the transaction that created it, and its
/// position in that transaction's output list.
///
/// The spending side of the same pair is
/// [`TransparentInput`](super::TransparentInput), which carries the identical
/// two fields. They are separate types because they answer different
/// questions — `Outpoint` names an output, `TransparentInput` names an input
/// that consumes one — and the distinction is what makes a signature like
/// "given these outpoints, which transactions spent them" read in the
/// direction it means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Outpoint {
    /// The transaction that created the output.
    pub txid: TransactionId,
    /// The output's index within that transaction.
    pub index: OutputIndex,
}
