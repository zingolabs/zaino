//! Query: locate the transaction that spent a given output.

use std::future::Future;

use zaino_primitives::types::rpc::{SpentInfo, SpentOutpoint};

use super::QueryError;

/// Domain error for [`GetSpentInfo`].
///
/// Both variants are *answers*, not failures: the question was asked and the
/// node said something definite. Neither is retryable.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetSpentInfoError {
    /// No spend of this output is on record.
    ///
    /// Covers three cases the interface does not distinguish: the output is
    /// unspent, the output is unknown, and the node has no spent index to
    /// consult. zcashd collapses all three into `-5 Unable to get spent info`,
    /// and nothing in the reply tells them apart, so neither can this.
    #[error("Unable to get spent info")]
    NotSpent,

    /// The validator does not implement `getspentinfo` at all.
    ///
    /// Distinct from [`NotSpent`](Self::NotSpent), which is an answer about
    /// *this* outpoint. This says no outpoint can be asked about here, so a
    /// caller seeing it should not conclude the output is unspent.
    #[error("this validator does not implement getspentinfo")]
    Unsupported,
}

/// Locate the transaction that spent a transparent output.
///
/// Maps to `getspentinfo`, which is a **zcashd-only** method: zebrad does not
/// implement it, and a zebrad-backed deployment answers every call with
/// [`Unsupported`](GetSpentInfoError::Unsupported).
///
/// # Absence is an error here, not `None`
///
/// Deliberately *not* `Result<Option<SpentInfo>, _>`, which is the shape its
/// neighbour [`GetTxOut`](crate::GetTxOut) correctly has. `gettxout` has a null
/// answer in the interface — an unspent output is a successful query returning
/// JSON `null`. `getspentinfo` has no such answer: zcashd reports "not spent"
/// as an error, and a client keys on the code. Modelling absence as `None` here
/// would force every consumer to invent a code on the way out, which is exactly
/// how this method came to report `-8` for a condition zcashd reports as `-5`.
///
/// # TODO: Zaino could answer this itself, and does not
///
/// This is a straight passthrough to the validator — it has been one since
/// `getspentinfo` was added, and no version of Zaino has ever answered it from
/// its own index. That is a problem now that zebrad is the supported backend,
/// because zebrad will never implement it: as things stand the method dies with
/// zcashd, even though Zaino is meant to take over from zcashd.
///
/// Zaino already indexes most of what the answer needs.
/// `FinalisedStateReader::get_outpoint_spender` (zaino-state, DB v1.2 and up)
/// maps an outpoint to the `TxLocation { block_height, tx_index }` that spent
/// it. That covers the response's `height` directly and its `txid` with one
/// further read. What is missing is the response's `index` — the position of
/// the spend within the spending transaction's input list — which the index
/// does not store and which would have to be recovered by scanning that
/// transaction's inputs for the outpoint.
///
/// Not implemented here because this crate is the port layer and has no
/// database, and not implemented in `zaino-state` in this change because it
/// would be new indexer capability rather than the rewire this work is doing.
/// Recorded so the gap is a decision rather than an oversight.
pub trait GetSpentInfo: Send + Sync {
    /// Locate an output's spender.
    fn get_spent_info(
        &self,
        outpoint: SpentOutpoint,
    ) -> impl Future<Output = Result<SpentInfo, QueryError<GetSpentInfoError>>> + Send;
}
