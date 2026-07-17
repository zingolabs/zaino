//! Capability: take a pinned snapshot of the best chain.

use std::future::Future;

use crate::error::PortError;
use crate::snapshot::ChainSnapshot;

/// Domain error for [`TakeSnapshot`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TakeSnapshotError {
    /// The engine is not ready to serve a snapshot (e.g. still syncing
    /// and no consistent view exists yet).
    #[error("the engine is not ready to serve a snapshot")]
    NotReady,
}

/// Take a pinned snapshot of the best chain.
///
/// The snapshot carries the port's strong guarantee: every read through
/// it observes the chain as of the pinned tip, and that data stays
/// readable while any clone of the snapshot lives — across reorgs. The
/// guarantee is unconditional (ADR 0003): an implementation must retain
/// the pinned view for as long as any clone lives, and an engine that
/// cannot is not an implementation of the port.
pub trait TakeSnapshot: Send + Sync {
    /// The pinned view this port hands out.
    type Snapshot: ChainSnapshot;

    /// Pin the current best chain and return a snapshot of it.
    fn take_snapshot(
        &self,
    ) -> impl Future<Output = Result<Self::Snapshot, PortError<TakeSnapshotError>>> + Send;
}
