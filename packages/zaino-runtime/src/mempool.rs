//! The mempool read model, as a component this runtime supervises.
//!
//! The mempool is two loops over one set. The core polls the validator and
//! publishes a retained set tagged with the validator tip it was read at. The
//! coherence layer compares that tag against the chain head's epoch and
//! publishes whether the set may be served to a caller holding a given view.
//!
//! Both run on this component's single task, so the runtime starts, stops and
//! restarts them together. Running them apart would let the coherence layer
//! reconcile against a core that is no longer polling, which publishes a verdict
//! about a set nobody is refreshing.
//!
//! # Why the chain head is observed rather than injected
//!
//! The mempool must not own or publish non-finalised state; it only needs that
//! state's epoch to judge coherence. [`ChainHeadEpochs`] adapts the chain head's
//! published cell onto [`NfsEpochObserver`], reading the epoch at the source so
//! the two cannot drift: relaying it through a second handle would let coherence
//! freeze against a tip nobody is being served.

use std::convert::Infallible;
use std::sync::Arc;

use tokio::sync::watch;
use zaino_chain_head::ports::ChainHeadBlockService as _;
use zaino_chain_head_service::ChainHeadSubscriber;
use zaino_component::{CancellationToken, Lifecycle, RunLoop, RunReporter};
use zaino_mempool::ports::{MempoolSource, NfsEpochObserver};
use zaino_mempool::MempoolConfig;
use zaino_mempool_service::{
    CoherenceService, CoherentSubscriber, MempoolService, MempoolSubscriber,
};
use zaino_primitives::types::ChainStateEpoch;

/// The chain head's epoch, as the coherence layer's observer.
///
/// `current_epoch` is the truth and is re-read on every reconcile. The wake
/// channel is a latency hint: without it the coherence layer notices a new
/// epoch only on its next tick, so every block is followed by a window in which
/// tip-coherent reads are frozen for no reason other than the poll cadence.
#[derive(Clone)]
pub struct ChainHeadEpochs {
    head: ChainHeadSubscriber,
    wake: watch::Receiver<()>,
}

impl NfsEpochObserver for ChainHeadEpochs {
    fn current_epoch(&self) -> Option<ChainStateEpoch> {
        Some(self.head.epoch())
    }

    fn subscribe_epoch_changes(&self) -> Option<watch::Receiver<()>> {
        Some(self.wake.clone())
    }
}

/// The mempool core and its coherence layer, supervised as one component.
pub struct MempoolComponent<S: MempoolSource> {
    core: Arc<MempoolService<S>>,
    coherence: Arc<CoherenceService<MempoolSubscriber, ChainHeadEpochs>>,
    /// The chain head's epoch feed, relayed into [`ChainHeadEpochs::wake`] by
    /// this component's own loop rather than by a task of its own.
    epochs: watch::Receiver<ChainStateEpoch>,
    wake: watch::Sender<()>,
}

impl<S: MempoolSource> MempoolComponent<S> {
    /// Build the component without starting either loop.
    ///
    /// The read handles are available at once, so the engine can be composed
    /// over them before the component is booted.
    pub fn new(source: S, head: ChainHeadSubscriber, config: MempoolConfig) -> Self {
        let (wake, wake_rx) = watch::channel(());
        let epochs = head.subscribe_updates();
        let observer = ChainHeadEpochs {
            head,
            wake: wake_rx,
        };

        // The services keep a token for their own `close`; a supervised run is
        // cancelled by the token the supervisor passes to `run`, so this one is
        // never fired here.
        let core = MempoolService::new(source, config.clone(), CancellationToken::new());
        let coherence = CoherenceService::new(
            core.subscriber(),
            observer,
            config,
            CancellationToken::new(),
        );

        Self {
            core,
            coherence,
            epochs,
            wake,
        }
    }

    /// The tip-agnostic read handle: the retained set and its change feed.
    pub fn mempool(&self) -> MempoolSubscriber {
        self.core.subscriber()
    }

    /// The tip-coherent read handle: the retained set plus whether it may be
    /// served to a caller holding a given epoch.
    pub fn coherent(&self) -> CoherentSubscriber {
        self.coherence.subscriber()
    }

    /// Relay each published epoch into the observer's wake channel until
    /// `cancel`. A lost send costs latency, never correctness — the epoch is
    /// re-read from the chain head on every reconcile.
    async fn relay_epochs(&self, cancel: CancellationToken) {
        let mut epochs = self.epochs.clone();
        loop {
            tokio::select! {
                () = cancel.cancelled() => return,
                changed = epochs.changed() => {
                    if changed.is_err() {
                        // The chain head is gone; there will be no more epochs.
                        return;
                    }
                    let _ = self.wake.send(());
                }
            }
        }
    }

    /// Report readiness once the core has published a set it actually read from
    /// the validator.
    ///
    /// The tag is the condition, not the set's size: an empty mempool is a
    /// perfectly good answer, so "has entries" would leave the component
    /// unready on a quiet chain. A set carrying a `source_tip` is one a poll
    /// produced, which is what a caller needs before a coherence judgement can
    /// mean anything.
    async fn announce_ready(
        core: Arc<MempoolService<S>>,
        reporter: RunReporter,
        cancel: CancellationToken,
    ) {
        let subscriber = core.subscriber();
        let mut updates = subscriber.subscribe_updates();
        loop {
            if subscriber.snapshot().source_tip().is_some() {
                reporter.ready();
                return;
            }
            tokio::select! {
                () = cancel.cancelled() => return,
                received = updates.recv() => {
                    if received.is_err() {
                        // Lagged or closed: fall through to re-read the status.
                        continue;
                    }
                }
            }
        }
    }
}

impl<S: MempoolSource> RunLoop for MempoolComponent<S> {
    /// Neither loop fails. A source error leaves the retained set in place and
    /// marks it incomplete, and the coherence layer freezes rather than giving
    /// up, so there is no failure for a supervisor to restart.
    type Error = Infallible;
    const LABEL: &'static str = "mempool";
    // A read model, not a server: it serves the set it holds while it catches
    // up, so the running-but-not-yet-`Ready` phase is `Syncing`.
    const RUNNING: Lifecycle = Lifecycle::Syncing;

    async fn run(
        self: Arc<Self>,
        cancel: CancellationToken,
        reporter: RunReporter,
    ) -> Result<(), Infallible> {
        // Joined, not selected: all three run until the token fires, and the
        // readiness watcher finishes early once it has reported. Selecting
        // would stop the loops the moment any one of them returned.
        tokio::join!(
            Arc::clone(&self.core).run_until(cancel.clone()),
            Arc::clone(&self.coherence).run_until(cancel.clone()),
            self.relay_epochs(cancel.clone()),
            Self::announce_ready(Arc::clone(&self.core), reporter, cancel.clone()),
        );
        Ok(())
    }
}
