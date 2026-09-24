//! Engine-level control capabilities — segregated so a read-only client need
//! not depend on `Broadcast`, a client that polls need not depend on
//! `TipSubscribe`, etc.

use std::future::Future;

use futures::stream::BoxStream;

use zaino_core::{
    MempoolTx, PassthroughAnswer, PassthroughQuery, PreIndexCompactTx, ReportedUpgrade,
    ServiceabilityManifest, TipEvent, TransactionId,
};

use crate::bundle::ChainSegment;
use crate::error::{BroadcastRejection, MempoolReadError, ReadError, Transient};

/// Pin the current view into a [`ChainSegment`].
///
/// The capture port both sides of the seam provide: the finalised store and the
/// non-finalised head each pin a coherent view, and the composer captures both
/// in one shot so the seam is coherent. The bound is [`ChainSegment`], not the
/// served [`Snapshot`](crate::Snapshot), so a non-finalised head — which has a
/// volatile window but no finalised boundary of its own — satisfies it without
/// having to fake one. A served view is a `Snapshot`, which *is* a
/// `ChainSegment`, so it satisfies this too.
pub trait TakeSnapshot: Send + Sync {
    type Snapshot: ChainSegment;
    fn snapshot(&self) -> impl Future<Output = Result<Self::Snapshot, Transient>> + Send;
}

/// Explicit tip-change subscription (ADR-0001): current tip first, then changes.
pub trait TipSubscribe: Send + Sync {
    fn subscribe_tip(&self) -> BoxStream<'_, TipEvent>;
}

/// Tip-tagged mempool stream, independent of chain-tip changes (ADR-0001).
pub trait MempoolSubscribe: Send + Sync {
    fn subscribe_mempool(&self) -> BoxStream<'_, MempoolTx>;
}

/// Live reads of individual mempool transactions, keyed by the txids a
/// [`MempoolSubscribe`] listing yields.
///
/// Separate from the snapshot's confirmed transaction read: the mempool is not
/// part of any pinned chain view, so these are answered live (unpinned) from the
/// same source that supplies the listing — never from a finalised secondary,
/// which holds no mempool. A transaction that left the mempool between listing
/// and fetch is a race, reported as `Ok(None)`, not a failure.
pub trait MempoolContent: Send + Sync {
    /// The raw bytes of one mempool transaction, or `None` if it is no longer in
    /// the mempool.
    fn mempool_raw_transaction(
        &self,
        txid: TransactionId,
    ) -> impl Future<Output = Result<Option<Vec<u8>>, MempoolReadError>> + Send;

    /// The compact projection of one mempool transaction, or `None` if it is no
    /// longer in the mempool. The projection is the source's concern (it needs
    /// the validator's chain library); the serving layer only forwards it.
    fn mempool_compact_transaction(
        &self,
        txid: TransactionId,
    ) -> impl Future<Output = Result<Option<PreIndexCompactTx>, MempoolReadError>> + Send;
}

/// Submit a transaction. Bytes in: a tx to relay is opaque to the engine — the
/// one honest bytes exception at the inner boundary (open question Q1).
pub trait Broadcast: Send + Sync {
    fn broadcast(
        &self,
        raw_tx: Vec<u8>,
    ) -> impl Future<Output = Result<TransactionId, BroadcastRejection>> + Send;
}

/// What is answerable *now*, given sync progress.
pub trait Serviceable: Send + Sync {
    fn serviceability(&self) -> ServiceabilityManifest;
}

/// The validator's network-upgrade schedule, passed through.
pub trait ReportedUpgrades: Send + Sync {
    fn reported_upgrades(
        &self,
    ) -> impl Future<Output = Result<Vec<ReportedUpgrade>, ReadError>> + Send;
}

/// Relay a node-operator query Zaino does not index (mining/peers/txoutset) to
/// the validator. A control, not a read: the answer comes from the source, not a
/// pinned snapshot, and is returned opaque. The node-rpc profile's delta.
pub trait Passthrough: Send + Sync {
    fn passthrough(
        &self,
        query: PassthroughQuery,
    ) -> impl Future<Output = Result<PassthroughAnswer, Transient>> + Send;
}
