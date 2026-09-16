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

use tokio::sync::watch;

use zaino_component::{CancellationToken, ReadySignal, SyncDriver};
use zaino_primitives::types::{Block, Height};
use zaino_source::{GetBlock, GetChainTip, SourceError, SubscribeChainTip, TipObservation};
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
        SourceError::Fetch(f) => IndexerError::Transport(f),
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
    S: GetBlock + GetChainTip + SubscribeChainTip + Send + Sync + 'static,
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

    /// A push subscription to the source's tip, or `None` if the source does not
    /// push (in which case the indexer stays at its initial catch-up height).
    pub fn subscribe_tip(&self) -> Option<watch::Receiver<TipObservation>> {
        self.source.subscribe_to_chain_tip()
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

impl<S, B, Ctx, F> SourceSyncDriver<S, B, Ctx, F>
where
    S: GetBlock + GetChainTip + SubscribeChainTip + Send + Sync + 'static,
    B: Backend + Send + Sync + 'static,
    Ctx: Send + Sync + 'static,
    F: Fn(Block) -> Ctx + Send + Sync + 'static,
{
    /// Provision `[from, to]` through the engine: the provisioner feeds a bounded
    /// channel which the engine drains, then both are joined typed.
    async fn sync_to(
        &self,
        engine: &mut SyncEngine<Ctx, B>,
        from: Height,
        to: Height,
    ) -> Result<(), IndexerError> {
        let (tx, rx) = mpsc::channel(self.channel_capacity);
        let provisioner = Arc::clone(&self.provisioner);
        let provision = tokio::spawn(async move { provisioner.provision(from, to, tx).await });
        // Dropping `tx` (moved into the task) at its end closes the channel, so
        // `sync_channel` returns once the range is drained.
        engine.sync_channel(rx).await?;
        provision.await??;
        Ok(())
    }
}

impl<S, B, Ctx, F> SyncDriver for SourceSyncDriver<S, B, Ctx, F>
where
    S: GetBlock + GetChainTip + SubscribeChainTip + Send + Sync + 'static,
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

        // Initial catch-up: sync [start, tip], then the component is Ready.
        let mut synced = self.provisioner.current_tip().await?;
        self.sync_to(&mut engine, self.start, synced).await?;
        caught_up.notify();

        // Steady-state follow: index each new range as the tip advances. If the
        // source does not push a tip (`None`), stay at the caught-up height.
        let Some(mut tips) = self.provisioner.subscribe_tip() else {
            cancel.cancelled().await;
            return Ok(());
        };
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                changed = tips.changed() => {
                    if changed.is_err() {
                        // The source stopped publishing; nothing more to follow.
                        return Ok(());
                    }
                    // Copy the height out before awaiting (drop the watch borrow).
                    let tip = tips.borrow_and_update().height;
                    if tip > synced {
                        let from = synced
                            .checked_add(1)
                            .expect("tip below max height has a successor");
                        self.sync_to(&mut engine, from, tip).await?;
                        synced = tip;
                    }
                }
            }
        }
    }
}
