//! One-shot mempool listing, for the node-RPC shape.
//!
//! Distinct from [`MempoolSubscribe`](crate::MempoolSubscribe), which is a
//! stream a wallet follows: node RPC asks "what is in the mempool right now",
//! once, and gets an answer. Both come from the source that serves the mempool,
//! never from a finalised secondary, which holds none.
//!
//! Three questions, three methods: the cheap txid listing (`getrawmempool`
//! without verbosity), the per-transaction detail the verbose form renders, and
//! the aggregate counts behind `getmempoolinfo`. The cheap listing reads the
//! txid port; the other two read the verbose metadata port, so a consumer that
//! only needs txids never pays the whole-mempool walk.

use std::future::Future;

use zaino_primitives::types::{Height, TransactionId, Zatoshis};

use crate::error::MempoolReadError;

/// One transaction in the mempool, with the detail the verbose listing renders.
///
/// The fields a node-RPC verbose `getrawmempool` entry carries, as domain types:
/// the fee is a typed [`Zatoshis`], not a rendered ZEC float — the adapter owns
/// that rendering at the wire boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MempoolEntry {
    /// The transaction's id.
    pub txid: TransactionId,
    /// Serialized byte length of the transaction.
    pub size: u64,
    /// The transaction's fee, in zatoshis.
    pub fee: Zatoshis,
    /// Unix time (seconds) the transaction entered the mempool, when the source
    /// reports one.
    pub entry_time: Option<i64>,
    /// Chain tip height when the transaction entered the mempool.
    pub entry_height: Height,
}

/// Aggregate mempool statistics — the domain behind `getmempoolinfo`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MempoolSummary {
    /// Number of transactions in the mempool.
    pub size: u64,
    /// Total serialized size of those transactions, in bytes.
    pub bytes: u64,
}

/// A point-in-time mempool listing.
pub trait MempoolListing: Send + Sync {
    /// `getrawmempool`: the txids currently in the mempool.
    fn mempool_txids(
        &self,
    ) -> impl Future<Output = Result<Vec<TransactionId>, MempoolReadError>> + Send;

    /// `getrawmempool verbose`: the per-transaction detail for the whole
    /// mempool.
    fn mempool_entries(
        &self,
    ) -> impl Future<Output = Result<Vec<MempoolEntry>, MempoolReadError>> + Send;

    /// `getmempoolinfo`: the count and total size of those transactions.
    fn mempool_summary(
        &self,
    ) -> impl Future<Output = Result<MempoolSummary, MempoolReadError>> + Send;
}
