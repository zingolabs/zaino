//! `zaino-indexer` — the index writer, wired as a runtime component.
//!
//! [`SyncEngineDriver`] adapts the #1402 [`SyncEngine`] to the runtime's
//! [`SyncDriver`] seam, so the Orchestra can boot and supervise index-building
//! as an `IndexerComponent` (Syncing → Ready, escalate on failure) — the same
//! lifecycle every other component gets, instead of a bespoke run loop.
//!
//! The concrete source-backed provisioner (over dev's `zaino-source`,
//! productionised from the sync-bench harness) will live here too; for now the
//! driver is generic over any [`Provisioner`], so it is exercised with the mock
//! provisioner + toy index set in tests.
#![forbid(unsafe_code)]

mod source_provisioner;
pub use source_provisioner::{SourceProvisioner, SourceSyncDriver};

use std::sync::{Arc, Mutex};

use zaino_component::{CancellationToken, ReadySignal, SyncDriver};
use zaino_sync::backend::Backend;
use zaino_sync::engine::SyncEngine;
use zaino_sync::primitives::BlockHeight;
use zaino_sync::provisioner::Provisioner;

/// What can go wrong driving the sync engine.
#[derive(Debug, thiserror::Error)]
pub enum IndexerError {
    /// The validator is unreachable after the resilient source's retry ladder is
    /// spent (`SourceError::Unavailable`). Kept distinct from other provisioning
    /// failures so the runtime can react to it as its own condition — the
    /// provisioner never re-implements retry.
    #[error("validator unavailable: {0}")]
    Unavailable(String),
    /// The provisioner could not supply block contexts.
    #[error("provisioning failed: {0}")]
    Provision(String),
    /// The engine failed to build the index.
    #[error("sync failed: {0}")]
    Sync(String),
    /// `run` was called after the engine had already been consumed.
    #[error("indexer already run")]
    AlreadyRun,
}

/// Drives a [`SyncEngine`] over a [`Provisioner`], presented as a
/// [`SyncDriver`].
///
/// This slice syncs once to a fixed `target` height then idles until cancelled;
/// steady-state tip-following (via dev's source subscriptions) lands with the
/// source-backed provisioner. The engine is consumed on the first `run`, so a
/// restart would need a rebuildable driver — fine under the current
/// escalate-not-restart policy.
pub struct SyncEngineDriver<Ctx, B: Backend, P> {
    engine: Mutex<Option<SyncEngine<Ctx, B>>>,
    provisioner: P,
    start: BlockHeight,
    target: BlockHeight,
}

impl<Ctx, B: Backend, P> SyncEngineDriver<Ctx, B, P>
where
    P: Provisioner<BlockContext = Ctx>,
{
    /// A driver that syncs `[start, target]` through `engine` from `provisioner`.
    pub fn new(
        engine: SyncEngine<Ctx, B>,
        provisioner: P,
        start: BlockHeight,
        target: BlockHeight,
    ) -> Self {
        Self {
            engine: Mutex::new(Some(engine)),
            provisioner,
            start,
            target,
        }
    }
}

impl<Ctx, B, P> SyncDriver for SyncEngineDriver<Ctx, B, P>
where
    Ctx: Send + Sync + 'static,
    B: Backend + Send + Sync + 'static,
    P: Provisioner<BlockContext = Ctx> + Send + Sync + 'static,
{
    type Error = IndexerError;

    async fn run(
        self: Arc<Self>,
        cancel: CancellationToken,
        caught_up: ReadySignal,
    ) -> Result<(), IndexerError> {
        let mut engine = self
            .engine
            .lock()
            .expect("engine mutex poisoned")
            .take()
            .ok_or(IndexerError::AlreadyRun)?;

        // Provision and build to the target. (sync_range is CPU-bound rayon
        // work; a source-backed driver will stream via sync_channel and offload
        // to spawn_blocking.)
        let blocks = self
            .provisioner
            .provision_range(self.start, self.target)
            .map_err(|e| IndexerError::Provision(e.to_string()))?;
        engine
            .sync_range(blocks)
            .map_err(|e| IndexerError::Sync(e.to_string()))?;

        // Caught up to the target — the component goes Ready. Then follow until
        // cancelled (a no-op until tip-following lands).
        caught_up.notify();
        cancel.cancelled().await;
        Ok(())
    }
}
