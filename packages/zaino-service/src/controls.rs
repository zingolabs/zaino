//! Engine-level control capabilities — segregated so a read-only client need
//! not depend on `Broadcast`, a client that polls need not depend on
//! `TipSubscribe`, etc.

use std::future::Future;

use futures::stream::BoxStream;

use zaino_core::{MempoolTx, ReportedUpgrade, ServiceabilityManifest, TipEvent, TransactionHash};

use crate::bundle::Snapshot;
use crate::error::{BroadcastRejection, ReadError, Transient};

/// Pin the current best chain into a [`Snapshot`].
pub trait TakeSnapshot: Send + Sync {
    type Snapshot: Snapshot;
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

/// Submit a transaction. Bytes in: a tx to relay is opaque to the engine — the
/// one honest bytes exception at the inner boundary (open question Q1).
pub trait Broadcast: Send + Sync {
    fn broadcast(
        &self,
        raw_tx: Vec<u8>,
    ) -> impl Future<Output = Result<TransactionHash, BroadcastRejection>> + Send;
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
