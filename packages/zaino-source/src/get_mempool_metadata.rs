//! Query: fetch per-transaction metadata for the whole mempool.

use std::future::Future;

use zaino_primitives::types::{Height, TransactionId};

use super::QueryError;

/// Per-transaction mempool metadata, as reported by the validator's own
/// mempool listing.
///
/// [`entry_height`](Self::entry_height) is the validator's authoritative chain
/// tip height at the moment the transaction entered its mempool — Zebra's
/// `VerifiedUnminedTx.height`, zcashd's `nHeight`. It is a protocol field the
/// validator owns, so a consumer must source it here rather than substituting a
/// locally derived value: the two disagree exactly when the chain moves under a
/// transaction, which is the case that matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MempoolTxMeta {
    /// The transaction's id.
    pub txid: TransactionId,
    /// Chain tip height when the transaction entered the validator's mempool.
    pub entry_height: Height,
    /// Unix time (seconds) the transaction entered the mempool, when the
    /// validator reports one.
    pub entry_time: Option<i64>,
}

/// Domain error for [`GetMempoolMetadata`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetMempoolMetadataError {
    /// This validator does not expose a mempool.
    ///
    /// The same condition [`GetMempoolTxidsError::Unavailable`] names, on the
    /// other listing method: the validator saying it does not implement the
    /// method, which is a statement about the node rather than about this
    /// request. Retrying will not help.
    ///
    /// [`GetMempoolTxidsError::Unavailable`]: super::GetMempoolTxidsError::Unavailable
    #[error("mempool unavailable")]
    Unavailable,
}

/// Fetch metadata for every transaction currently in the mempool.
///
/// Maps to `getrawmempool` with `verbose = true` over JSON-RPC.
///
/// # Cost
///
/// This is a **whole-mempool walk** on a real validator, and is far more
/// expensive than [`GetMempoolTxids`](super::GetMempoolTxids). Consumers should
/// diff with the cheap txid listing and reach for this only when that diff
/// shows additions, and should coalesce repeated calls rather than issuing one
/// per poll.
pub trait GetMempoolMetadata: Send + Sync {
    /// Fetch mempool metadata.
    fn get_mempool_metadata(
        &self,
    ) -> impl Future<Output = Result<Vec<MempoolTxMeta>, QueryError<GetMempoolMetadataError>>> + Send;
}
