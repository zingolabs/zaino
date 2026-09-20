//! `zaino-indexer` — the index writer, wired as a runtime component.
//!
//! [`SyncEngineDriver`] adapts the #1402 [`SyncEngine`] to the runtime's
//! [`RunLoop`] seam, so the Orchestra can boot and supervise index-building
//! as an `IndexerComponent` (Syncing → Ready, escalate on failure) — the same
//! lifecycle every other component gets, instead of a bespoke run loop.
//!
//! The concrete source-backed provisioner (over dev's `zaino-source`,
//! productionised from the sync-bench harness) will live here too; for now the
//! driver is generic over any [`Provisioner`], so it is exercised with the mock
//! provisioner + toy index set in tests.
#![forbid(unsafe_code)]

mod source_provisioner;
pub use source_provisioner::{
    CompactBlocks, FetchConcurrency, FullBlocks, SourceFetch, SourceProvisioner, SourceSyncDriver,
    SyncTuning,
};

use std::sync::{Arc, Mutex};

use zaino_component::{CancellationToken, Lifecycle, RunLoop, RunReporter};
use zaino_primitives::types::Height;
use zaino_sync::backend::Backend;
use zaino_sync::engine::SyncEngine;
use zaino_sync::primitives::BlockHeight;
use zaino_sync::provisioner::Provisioner;

/// What can go wrong driving the sync engine.
///
/// Each variant chains the underlying typed error as its `source` — nothing is
/// stringified, so the cause is inspectable and matchable. Only the source's
/// generic `Domain(E)` case is boxed (a single non-generic `Error` type cannot
/// hold every `E`); the operationally-important axes (unavailable, transport,
/// sync) stay fully typed.
#[derive(Debug, thiserror::Error)]
pub enum IndexerError {
    /// The source is unreachable after the resilient source's retry ladder is
    /// spent (`SourceError::Unavailable`). Distinct so the runtime can react to
    /// it as its own condition — the provisioner never re-implements retry.
    #[error(transparent)]
    SourceUnreachable(#[from] zaino_source::UnavailableError),
    /// A non-retryable transport failure reaching the source.
    #[error(transparent)]
    Transport(#[from] zaino_source::NonDomainError),
    /// The source answered with a domain-level rejection. Boxed because the
    /// source's domain error is generic; the cause chain is preserved.
    #[error("source rejected the request")]
    Domain(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// The batch provisioner could not supply block contexts.
    #[error(transparent)]
    Provision(#[from] zaino_sync::provisioner::ProvisionError),
    /// The engine failed to build the index.
    #[error(transparent)]
    Sync(#[from] zaino_sync::engine::SyncError),
    /// A worker task the indexer spawned terminated **unexpectedly** — it
    /// panicked or was aborted, rather than returning a value. This is the
    /// internal-fault channel (distinct from the actionable source/sync errors
    /// above): a bug or a forced abort, not a defined indexing failure — which is
    /// why the name states the epistemic outright rather than only the mechanism.
    /// The [`TaskError`] names the worker (our name, not tokio's runtime id) and
    /// keeps a panic's message, so a health `reason` still says *what* failed;
    /// the panic's origin is separately logged by the panic hook the moment it
    /// happens (see `zaino_logging`).
    #[error(transparent)]
    UnexpectedWorkerFailure(#[from] zaino_async::TaskError),
    /// `run` was called after the engine had already been consumed.
    #[error("indexer already run")]
    AlreadyRun,
}

/// Where a sync run begins, derived from the backend's committed watermark.
///
/// Formalises **fresh vs mid-sync**: a backend with no watermark is [`Fresh`]
/// and indexes from genesis; one with a watermark [`Resume`]s just after it.
/// This is *only* about where to start. Whether the persisted indexes are
/// themselves compatible with this build is a separate concern — the engine
/// rejects an incompatible index at construction; that rejection is not mixed in
/// here.
///
/// [`Fresh`]: SyncStart::Fresh
/// [`Resume`]: SyncStart::Resume
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncStart {
    /// No watermark recorded: a fresh backend. Index from genesis.
    Fresh,
    /// The backend is committed up to this [`Height`] (inclusive). Resume after it.
    Resume(Height),
}

impl SyncStart {
    /// The height indexing should begin at: genesis for a fresh backend, or one
    /// past the committed tip on resume.
    pub fn next_height(self) -> Height {
        match self {
            SyncStart::Fresh => Height::GENESIS,
            SyncStart::Resume(committed) => committed
                .checked_add(1)
                .expect("resume height within the protocol limit"),
        }
    }
}

/// Assess where indexing should begin for `backend`, from its committed
/// watermark.
///
/// Reads only the shared watermark seam — it does **not** validate index
/// formats. Format compatibility is the engine's concern: it rejects an
/// incompatible index when it loads state, independently of this.
pub fn assess_start<B: Backend>(backend: &B) -> Result<SyncStart, IndexerError> {
    let reader = backend.reader().map_err(|e| IndexerError::Sync(e.into()))?;
    Ok(
        match zaino_persistence_codec::watermark::read(&reader)
            .map_err(|e| IndexerError::Sync(e.into()))?
        {
            Some(committed) => SyncStart::Resume(committed),
            None => SyncStart::Fresh,
        },
    )
}

/// Drives a [`SyncEngine`] over a [`Provisioner`], presented as a
/// [`RunLoop`].
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

impl<Ctx, B, P> RunLoop for SyncEngineDriver<Ctx, B, P>
where
    Ctx: Send + Sync + 'static,
    B: Backend + Send + Sync + 'static,
    P: Provisioner<BlockContext = Ctx> + Send + Sync + 'static,
{
    type Error = IndexerError;
    const LABEL: &'static str = "run loop";
    const RUNNING: Lifecycle = Lifecycle::Syncing;

    async fn run(
        self: Arc<Self>,
        cancel: CancellationToken,
        reporter: RunReporter,
    ) -> Result<(), IndexerError> {
        let mut engine = self
            .engine
            .lock()
            .expect("engine mutex poisoned")
            .take()
            .ok_or(IndexerError::AlreadyRun)?;

        // Provision and build to the target. (sync_range is CPU-bound rayon
        // work; a source-backed driver will stream via sync_channel and offload
        // to spawn_blocking.) Errors chain typed via `?` — nothing stringified.
        let blocks = self.provisioner.provision_range(self.start, self.target)?;
        engine.sync_range(blocks)?;

        // Caught up to the target — the component goes Ready. Then follow until
        // cancelled (a no-op until tip-following lands).
        reporter.ready();
        cancel.cancelled().await;
        Ok(())
    }
}
