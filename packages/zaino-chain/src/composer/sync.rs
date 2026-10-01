//! Driving the finalised store from the chain head's freeze stream.
//!
//! The one place in this crate that *writes*. Everything else composes reads
//! from providers someone else advances; this advances one of them.
//!
//! # Why the composer drives it, and not the store
//!
//! A chain store is built to where it is told to be built to. It has a
//! validator of its own and could follow the chain unaided, but a store that
//! decided its own target would be a second writer racing whatever else in the
//! deployment thought it owned that decision — and the two would contend on one
//! database, multiplying memory rather than throughput.
//!
//! The composer is the party that knows. It already holds both tiers, already
//! knows where the seam between them is, and is the thing that suffers if they
//! drift apart. So the store stays caller-driven and this is the caller.
//!
//! # The loop is the gap error
//!
//! There is no separate "catch up first, then follow" phase, because the
//! catch-up *is* the ordinary failure of the following step. The chain head
//! emits a block once it falls below the consensus seam; the store accepts one
//! only at `tip + 1`. An empty store handed a block from the middle of the
//! chain therefore answers
//! [`FreezeGap`](zaino_chain_store::ChainStoreError::FreezeGap), which carries
//! the height to build to — and building to it is exactly the initial sync.
//!
//! So a cold start and a chain head that re-anchored after an outage take the
//! same path, and it is the path the tests exercise on every run rather than a
//! startup branch nothing reaches twice.
//!
//! # What this is allowed to lose
//!
//! The freeze stream is best-effort by contract: it is a `broadcast`, so a slow
//! consumer is told it lagged rather than blocking the chain head, and a chain
//! head that re-anchors simply never emits what it skipped. Neither is handled
//! specially here, because neither needs to be — a missed block becomes a gap
//! on the next freeze, and the gap repairs itself. Losing blocks costs a fetch,
//! not correctness.

use std::sync::{Arc, Mutex};

use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use zaino_chain_head::{ChainHeadBlock, ChainHeadBlockService, ChainHeadFreezeEvents};
use zaino_chain_store::{
    ChainStoreError, ChainStoreFreezeSink, ChainStoreIngest, ChainStoreService, FrozenBlock,
};
use zaino_component::{
    ComponentName, ComponentStatus, Health, Lifecycle, StatusSource, StatusWatch,
};

use crate::block::frozen_block;
use crate::composer::ChainViewComposer;
use crate::source::ChainViewSource;

/// How this subsystem names itself in status reports.
const COMPONENT: ComponentName = ComponentName("chain-view-sync");

/// This loop's status, as the two axes a component reports.
///
/// Both tracked natively rather than folded into one value and split again:
/// this is new code, so there is no fused form to be compatible with, and the
/// distinction is one the loop genuinely makes. A gap it cannot close leaves it
/// `Syncing` and `Recoverable` — still doing its job, not yet succeeding —
/// which one axis cannot say.
fn report(lifecycle: Lifecycle, health: Health) -> ComponentStatus {
    ComponentStatus::new(COMPONENT, lifecycle, health)
}

/// The most blocks handed to one `freeze`.
///
/// A freeze is one write transaction and one durability barrier, so batching
/// amortises both — but an unbounded batch would hold the whole retention
/// window in memory during a catch-up, and the window is the one thing here
/// sized by a config this loop cannot see. A cap keeps the memory bounded
/// without needing to know it.
const MAX_BATCH: usize = 64;

/// A running sync loop.
///
/// Returned rather than kept on the composer because the composer is `Clone`
/// and a read handle: a loop reachable from every clone would be a loop any
/// clone could stop. One owner, one lifetime.
#[derive(Debug)]
pub struct ChainViewSync {
    status: watch::Sender<ComponentStatus>,
    cancel: CancellationToken,
    /// `None` once [`shutdown`](Self::shutdown) has taken it.
    task: Mutex<Option<JoinHandle<()>>>,
}

impl ChainViewSync {
    /// How the loop is faring.
    ///
    /// `Syncing` while a gap is being repaired, `Ready` once freezes are
    /// landing, `Offline` once the loop has stopped. The store's own status is
    /// separate and says whether the *database* is healthy; this says whether
    /// anything is still feeding it.
    pub fn status(&self) -> ComponentStatus {
        *self.status.borrow()
    }

    /// Stops the loop.
    ///
    /// The token passed to
    /// [`spawn_sync`](ChainViewComposer::spawn_sync) also stops it; this
    /// additionally publishes `Closing` and releases the handle, so shutdown is
    /// observable rather than merely effective. The same shape as
    /// `ChainHeadService::shutdown`, deliberately: a deployment shutting both
    /// down should not have to remember which one behaves differently.
    ///
    /// Synchronous, and does not wait: it cancels, then aborts. Safe rather
    /// than merely expedient — a batch is written by `freeze`, which is atomic
    /// at the store, so a task killed part-way leaves the store at a block
    /// boundary either way.
    pub fn shutdown(&self) {
        self.status
            .send_replace(report(Lifecycle::Closing, Health::Healthy));
        self.cancel.cancel();
        if let Some(handle) = self
            .task
            .lock()
            .expect("chain view sync task mutex poisoned")
            .take()
        {
            handle.abort();
        }
    }
}

impl StatusSource for ChainViewSync {
    fn status(&self) -> ComponentStatus {
        ChainViewSync::status(self)
    }
}

/// So a supervisor reacts to a transition rather than polling for one.
impl StatusWatch for ChainViewSync {
    fn subscribe(&self) -> watch::Receiver<ComponentStatus> {
        self.status.subscribe()
    }
}

/// Stopping the loop when its handle goes, so a dropped handle cannot leave a
/// task writing to a store nobody is reading.
impl Drop for ChainViewSync {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl<Store, Head, Source> ChainViewComposer<Store, Head, Source>
where
    Store: ChainStoreService + ChainStoreIngest + ChainStoreFreezeSink + 'static,
    Head: ChainHeadBlockService + ChainHeadFreezeEvents,
    Source: ChainViewSource,
{
    /// Starts feeding this view's finalised store from its chain head.
    ///
    /// # Why this is a method and not the default
    ///
    /// A deployment that drives its own store — building it from a validator on
    /// its own schedule — must not also get this, or two writers contend on one
    /// database. So the loop is asked for rather than assumed, and the ask is
    /// this call.
    ///
    /// # Why it is not a feature flag
    ///
    /// The bounds already do the work a flag would, and do it per deployment
    /// rather than per build: a composer whose store cannot ingest or whose
    /// chain head publishes no freeze events does not have this method at all.
    /// A cargo feature would be worse on both counts — features are additive
    /// across a workspace, so one crate enabling it turns it on for every other
    /// consumer of this crate, and it would decide at compile time a question
    /// that belongs to the deployment.
    ///
    /// # Shutdown
    ///
    /// Cancelling `cancel` stops the loop, as does dropping the returned
    /// handle. Both are the same mechanism; the handle also makes the stop
    /// observable through [`ChainViewSync::status`].
    pub fn spawn_sync(self: &Arc<Self>, cancel: CancellationToken) -> Arc<ChainViewSync> {
        let status = watch::Sender::new(report(Lifecycle::Syncing, Health::Healthy));

        // Subscribed before the task starts. A `broadcast` delivers only what
        // is sent after a receiver exists, so subscribing inside the task would
        // drop every block frozen between this call returning and the task
        // first being polled — a window that is short, unbounded, and invisible.
        let frozen = self.head.subscribe_frozen();

        let task = tokio::spawn(run(
            Arc::clone(self),
            frozen,
            status.clone(),
            cancel.clone(),
        ));

        Arc::new(ChainViewSync {
            status,
            cancel,
            task: Mutex::new(Some(task)),
        })
    }

    /// Writes one batch, repairing a gap once if the store reports one.
    ///
    /// Once, not until it succeeds: a second gap means the chain moved while
    /// the build was running, which the next batch will report again with a
    /// nearer target. Looping here would block the loop on a chain that is
    /// still moving, and lose the cancellation check while doing it.
    async fn freeze_batch(&self, batch: &[FrozenBlock], status: &watch::Sender<ComponentStatus>) {
        let Err(error) = self.store.freeze(batch).await else {
            status.send_replace(report(Lifecycle::Ready, Health::Healthy));
            return;
        };

        let ChainStoreError::FreezeGap {
            store_tip,
            first_frozen,
        } = error
        else {
            // Not a gap, so not this loop's to repair. The store's own status
            // reports a broken database; the loop keeps running because the
            // next batch may well land.
            warn!(%error, "freezing a batch failed");
            status.send_replace(report(status.borrow().lifecycle, Health::Recoverable));
            return;
        };

        let Some(target) = first_frozen.checked_sub(1) else {
            // `first_frozen` is genesis and the store is below it, which means
            // the store is empty and there is nothing below to build. Writing
            // genesis itself is the next freeze's job.
            return;
        };

        info!(
            ?store_tip,
            %first_frozen,
            %target,
            "chain head is ahead of the store; building to close the gap",
        );
        status.send_replace(report(Lifecycle::Syncing, Health::Healthy));

        if let Err(error) = self.store.build_to(target).await {
            warn!(%error, %target, "building to close a freeze gap failed");
            status.send_replace(report(Lifecycle::Syncing, Health::Recoverable));
            return;
        }

        match self.store.freeze(batch).await {
            Ok(()) => {
                status.send_replace(report(Lifecycle::Ready, Health::Healthy));
            }
            // The chain moved while the build ran. Reported at debug because it
            // is the expected outcome of catching up to a live chain, not a
            // fault: the next batch carries a nearer target.
            Err(ChainStoreError::FreezeGap { first_frozen, .. }) => {
                debug!(%first_frozen, "the gap moved while it was being closed");
            }
            Err(error) => warn!(%error, "freezing a batch after closing a gap failed"),
        }
    }
}

/// The loop.
async fn run<Store, Head, Source>(
    composer: Arc<ChainViewComposer<Store, Head, Source>>,
    mut frozen: tokio::sync::broadcast::Receiver<ChainHeadBlock>,
    status: watch::Sender<ComponentStatus>,
    cancel: CancellationToken,
) where
    Store: ChainStoreService + ChainStoreIngest + ChainStoreFreezeSink + 'static,
    Head: ChainHeadBlockService + ChainHeadFreezeEvents,
    Source: ChainViewSource,
{
    loop {
        let first = tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            received = frozen.recv() => received,
        };

        let first = match first {
            Ok(block) => block,
            // Told exactly how many were missed, which is worth recording —
            // but not worth acting on. The next block will not sit at
            // `tip + 1`, so the store reports a gap and the repair path runs.
            Err(RecvError::Lagged(missed)) => {
                warn!(
                    missed,
                    "fell behind the freeze stream; the gap will be rebuilt"
                );
                continue;
            }
            // The chain head is gone. Nothing will ever be frozen again, so
            // the loop has no reason to stay up.
            Err(RecvError::Closed) => {
                info!("the chain head closed its freeze stream");
                break;
            }
        };

        composer
            .freeze_batch(&drain(&mut frozen, first), &status)
            .await;
    }

    status.send_replace(report(Lifecycle::Offline, Health::Offline));
}

/// `first`, plus whatever else is already waiting, up to [`MAX_BATCH`].
///
/// Non-blocking after the first: a batch is worth forming only from blocks
/// already in hand, because waiting to fill one would delay the write that is
/// ready in order to amortise a write that has not happened.
fn drain(
    frozen: &mut tokio::sync::broadcast::Receiver<ChainHeadBlock>,
    first: ChainHeadBlock,
) -> Vec<FrozenBlock> {
    let mut batch = vec![frozen_block(&first.block, first.tree_roots)];

    while batch.len() < MAX_BATCH {
        match frozen.try_recv() {
            Ok(block) => batch.push(frozen_block(&block.block, block.tree_roots)),
            // Empty, closed, or lagged: all three mean "no more contiguous
            // blocks to add right now". Closed and lagged are the outer loop's
            // to report, and it will see them on its next `recv`.
            Err(TryRecvError::Empty | TryRecvError::Closed | TryRecvError::Lagged(_)) => break,
        }
    }

    batch
}
