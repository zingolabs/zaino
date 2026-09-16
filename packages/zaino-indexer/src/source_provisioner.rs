//! The source-backed provisioner and its streaming driver.
//!
//! The provisioner is the supply half of the indexer (see the design note
//! "provisioner is internal to the indexer"): it fetches blocks from a validator
//! and projects them into the set-wide context the engine consumes. It is
//! **generic over the source** — bound on exactly the `zaino-source` capability
//! traits it needs, so any validator adapter (zebra-rpc, zebra-readstate, a
//! mock) plugs in — and it reacts to a typed source error rather than baking in
//! retry (transient handling is the resilient-source decorator's job, below).
//!
//! [`SourceSyncDriver`] wires it to the engine: it spawns the provisioner
//! feeding the engine's `sync_channel`, and reports `Ready` once caught up to the
//! source tip. Steady-state tip-following (via `SubscribeChainTip`) is the next
//! increment; this drives the initial sync to the current tip.

use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use zaino_component::{CancellationToken, ReadySignal, SyncDriver};
use zaino_primitives::types::{Block, Height};
use zaino_source::{GetBlock, GetChainTip, SourceError};
use zaino_sync::backend::Backend;
use zaino_sync::engine::SyncEngine;

use crate::IndexerError;

/// Map a resilient-port [`SourceError`] onto an indexer error, preserving each
/// cause typed (no stringification). `Unavailable` and `Fetch` are concrete;
/// only the generic `Domain(E)` is boxed, since one non-generic `IndexerError`
/// cannot hold every `E`. No wildcard arm: a new `SourceError` variant must be
/// classified here.
fn map_source<E: std::error::Error + Send + Sync + 'static>(err: SourceError<E>) -> IndexerError {
    match err {
        SourceError::Unavailable(u) => IndexerError::SourceUnreachable(u),
        SourceError::Fetch(f) => IndexerError::Fetch(f),
        SourceError::Domain(d) => IndexerError::Domain(Box::new(d)),
    }
}

/// Fetches blocks from a validator source and projects them into the engine's
/// set-wide context `Ctx` via `build`.
///
/// Generic over the source `S` (capability-bound) and the projection `build`,
/// so the same provisioner serves any adapter and any index set.
pub struct SourceProvisioner<S, Ctx, F> {
    source: Arc<S>,
    build: F,
    _ctx: std::marker::PhantomData<fn() -> Ctx>,
}

impl<S, Ctx, F> SourceProvisioner<S, Ctx, F>
where
    S: GetBlock + GetChainTip + Send + Sync + 'static,
    F: Fn(Block) -> Ctx + Send + Sync + 'static,
    Ctx: Send + 'static,
{
    /// A provisioner over `source`, projecting each fetched block with `build`.
    pub fn new(source: Arc<S>, build: F) -> Self {
        Self {
            source,
            build,
            _ctx: std::marker::PhantomData,
        }
    }

    /// The validator's current tip height.
    pub async fn current_tip(&self) -> Result<Height, IndexerError> {
        self.source
            .get_chain_tip()
            .await
            .map(|(_hash, height)| height)
            .map_err(map_source)
    }

    /// Fetch `[from, to]` and send each projected context into `tx`, in order.
    /// Stops early (Ok) if the receiver is dropped — the engine has gone away.
    pub async fn provision(
        &self,
        from: Height,
        to: Height,
        tx: mpsc::Sender<Ctx>,
    ) -> Result<(), IndexerError> {
        for h in u32::from(from)..=u32::from(to) {
            // `h` lies within `[from, to]`, both valid `Height`s, so it cannot
            // exceed the max height — the conversion is infallible by construction.
            let height = Height::try_from(h).expect("height within a valid range is valid");
            let block = self.source.get_block(height).await.map_err(map_source)?;
            let ctx = (self.build)(block);
            if tx.send(ctx).await.is_err() {
                // Receiver dropped: the engine stopped consuming; nothing to do.
                return Ok(());
            }
        }
        Ok(())
    }
}

/// Drives a [`SyncEngine`] from a [`SourceProvisioner`], presented as a
/// [`SyncDriver`]. The provisioner streams into the engine's `sync_channel`; the
/// component reaches `Ready` once the engine has consumed up to the source tip.
pub struct SourceSyncDriver<S, B: Backend, Ctx, F> {
    engine: Mutex<Option<SyncEngine<Ctx, B>>>,
    provisioner: Arc<SourceProvisioner<S, Ctx, F>>,
    start: Height,
    channel_capacity: usize,
}

impl<S, B: Backend, Ctx, F> SourceSyncDriver<S, B, Ctx, F> {
    /// A driver syncing from `start` to the source tip, buffering up to
    /// `channel_capacity` contexts between the provisioner and the engine.
    pub fn new(
        engine: SyncEngine<Ctx, B>,
        provisioner: Arc<SourceProvisioner<S, Ctx, F>>,
        start: Height,
        channel_capacity: usize,
    ) -> Self {
        Self {
            engine: Mutex::new(Some(engine)),
            provisioner,
            start,
            channel_capacity,
        }
    }
}

impl<S, B, Ctx, F> SyncDriver for SourceSyncDriver<S, B, Ctx, F>
where
    S: GetBlock + GetChainTip + Send + Sync + 'static,
    B: Backend + Send + Sync + 'static,
    Ctx: Send + Sync + 'static,
    F: Fn(Block) -> Ctx + Send + Sync + 'static,
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

        let tip = self.provisioner.current_tip().await?;

        // Provision [start, tip] into the engine's channel; dropping `tx` at the
        // end closes the channel so `sync_channel` returns once drained.
        let (tx, rx) = mpsc::channel(self.channel_capacity);
        let provisioner = Arc::clone(&self.provisioner);
        let start = self.start;
        let provision = tokio::spawn(async move { provisioner.provision(start, tip, tx).await });

        engine.sync_channel(rx).await?;

        // Surface a provisioning failure (the channel closed early because the
        // provisioner errored, not because it finished). `??`: the task's
        // `JoinError` and the inner `IndexerError` both propagate typed.
        provision.await??;

        // Caught up to the tip. (Tip-following via SubscribeChainTip is next.)
        caught_up.notify();
        cancel.cancelled().await;
        Ok(())
    }
}
