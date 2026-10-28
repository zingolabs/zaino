//! `getspentinfo` — locate the transaction that spent a given output.

use crate::types::{Height, OutputIndex, TransactionId};

/// The output whose spender is being requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpentOutpoint {
    /// Transaction containing the output.
    pub txid: TransactionId,
    /// Index of the output within that transaction's outputs.
    pub index: OutputIndex,
}

/// Where a transparent output was spent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpentInfo {
    /// The transaction that spent the output.
    pub txid: TransactionId,
    /// Index of the spending input within that transaction's inputs.
    pub index: OutputIndex,
    /// Height of the block containing the spending transaction.
    ///
    /// Required rather than optional: an output is only "spent" once the
    /// spending transaction is mined, so a validator answering this call at
    /// all can name the height. Mempool spends are not reported here.
    pub height: Height,
}
