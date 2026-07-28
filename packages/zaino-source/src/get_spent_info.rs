//! Query: locate the transaction that spent a given output.

use std::future::Future;

use zaino_primitives::types::rpc::{SpentInfo, SpentOutpoint};

use super::QueryError;

/// Domain error for [`GetSpentInfo`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetSpentInfoError {
    /// The validator does not maintain a spent index, so the question cannot
    /// be answered on this node at all.
    #[error("spent index unavailable")]
    IndexUnavailable,
}

/// Locate the transaction that spent a transparent output.
///
/// `Ok(None)` means the output is unspent, or the validator has no record of
/// it — the ordinary answer, distinct from
/// [`IndexUnavailable`](GetSpentInfoError::IndexUnavailable), which says the
/// node cannot answer this class of question.
///
/// Maps to `getspentinfo` over JSON-RPC.
pub trait GetSpentInfo: Send + Sync {
    /// Locate an output's spender.
    fn get_spent_info(
        &self,
        outpoint: SpentOutpoint,
    ) -> impl Future<Output = Result<Option<SpentInfo>, QueryError<GetSpentInfoError>>> + Send;
}
