//! The tip-aware coherence layer (gated behind `tip_aware_mempool`).
//!
//! Wraps a tip-agnostic [`Mempool`] core and an [`NfsEpochObserver`] and publishes
//! a [`CoherentSnapshot`]: the core set made coherent with Zaino's
//! non-finalized-state (NS) tip. Combined ChainIndex reads (`get_raw_transaction`,
//! `get_transaction_status`) and the raw-transaction stream consult it so they
//! only serve the mempool when it matches the caller's NS snapshot.
//!
//! # No re-fetch: coherence is a pure function of (core set + V, NS)
//!
//! The core tags every published set with the validator tip `V` it was fetched at
//! ([`MempoolSnapshot::source_tip`](zaino_mempool::snapshot::MempoolSnapshot::source_tip)).
//! Because that tag and the mempool data are a
//! single-source pair, the coherence layer never re-fetches and needs no
//! before/after guards: it simply compares `V` against the observed NS epoch. When
//! they agree it blesses the core's *current* set as valid for that epoch; when
//! they diverge (or a tip is unavailable, or the core set is incomplete) it freezes
//! at the last blessed set. This is the guarantee the whole rework rests on — see
//! `zaino-mempool`'s `tip` module docs.

use std::sync::Arc;
use std::time::Instant;

use arc_swap::ArcSwap;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use zaino_status::{NamedAtomicStatus, StatusType};

use zaino_mempool::config::MempoolConfig;
use zaino_mempool::event::MempoolEvent;
use zaino_mempool::ports::{
    Mempool, MempoolStreamError, NfsEpochObserver, NoNfs, NonFinalizedEpoch,
};
use zaino_mempool::tip::CoherentSnapshot;

mod publish;
mod reconcile;
mod run;

/// Writer-local state for synthesizing a non-finalized epoch from the validator
/// tip in validator-only mode: `generation` increments only when the validator
/// tip hash changes, giving a stable epoch for a stable tip.
#[derive(Default)]
struct SynthesizedEpochState {
    last_validator_hash: Option<zaino_primitives::types::BlockHash>,
    generation: u64,
}

/// The tip-aware coherence service.
///
/// Generic over the core mempool port `M` and the NS-epoch observer `N`, so it has
/// no `zaino-state` dependency. With an observer ([`Self::spawn`]) it enforces
/// dual-tip coherence between the validator tip and Zaino's NS tip; without one
/// ([`Self::spawn_validator_only`]) it mirrors the validator alone (single-tip),
/// synthesizing the epoch from the validator tip.
pub struct CoherenceService<M: Mempool, N: NfsEpochObserver> {
    mempool: M,
    nfs: Option<N>,
    synth_epoch: std::sync::Mutex<SynthesizedEpochState>,
    coherent: Arc<ArcSwap<CoherentSnapshot>>,
    events: broadcast::Sender<Arc<MempoolEvent>>,
    config: MempoolConfig,
    status: NamedAtomicStatus,
    /// When the current continuous freeze began, or `None` when the view is
    /// live/serving. Shared with every [`CoherentSubscriber`] so an operator can
    /// escalate a freeze that outlasts normal block-driven thaw (see
    /// [`CoherentSubscriber::frozen_for`]). Set on the transition *into* a freeze,
    /// held across repeated freezes, cleared on thaw.
    frozen_since: Arc<std::sync::Mutex<Option<Instant>>>,
    cancel: CancellationToken,
    task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl<M: Mempool, N: NfsEpochObserver> std::fmt::Debug for CoherenceService<M, N> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoherenceService")
            .field("status", &self.status.load())
            .finish_non_exhaustive()
    }
}

impl<M: Mempool> CoherenceService<M, NoNfs> {
    /// Spawn a validator-only coherence layer: coherence collapses to single-tip
    /// (freeze on validator-tip change); the epoch is synthesized from the
    /// validator tip.
    pub fn spawn_validator_only(
        mempool: M,
        config: MempoolConfig,
        cancel: CancellationToken,
    ) -> Arc<Self> {
        Self::spawn_inner(mempool, None, config, cancel)
    }
}

impl<M: Mempool, N: NfsEpochObserver> CoherenceService<M, N> {
    /// Spawn the coherence layer against the given non-finalized-state observer.
    pub fn spawn(
        mempool: M,
        nfs: N,
        config: MempoolConfig,
        cancel: CancellationToken,
    ) -> Arc<Self> {
        Self::spawn_inner(mempool, Some(nfs), config, cancel)
    }

    fn spawn_inner(
        mempool: M,
        nfs: Option<N>,
        config: MempoolConfig,
        cancel: CancellationToken,
    ) -> Arc<Self> {
        let (events, _) = broadcast::channel(config.event_buffer_len());

        let service = Arc::new(Self {
            mempool,
            nfs,
            synth_epoch: std::sync::Mutex::new(SynthesizedEpochState::default()),
            coherent: Arc::new(ArcSwap::from_pointee(CoherentSnapshot::empty_not_ready())),
            events,
            config,
            status: NamedAtomicStatus::new("MempoolCoherence", StatusType::Spawning),
            frozen_since: Arc::new(std::sync::Mutex::new(None)),
            cancel,
            task: std::sync::Mutex::new(None),
        });

        let task_service = Arc::clone(&service);
        let handle = tokio::spawn(async move {
            task_service.run().await;
        });

        *service.task.lock().expect("coherence task lock poisoned") = Some(handle);

        service
    }

    /// A cheap, cloneable read handle onto the coherent mempool view.
    pub fn subscriber(&self) -> CoherentSubscriber {
        CoherentSubscriber {
            coherent: Arc::clone(&self.coherent),
            events: self.events.clone(),
            status: self.status.clone(),
            frozen_since: Arc::clone(&self.frozen_since),
        }
    }

    /// Current coherence-layer status.
    pub fn status(&self) -> StatusType {
        self.status.load()
    }

    /// Signal shutdown: publish `Closing`, then stop the task.
    pub fn close(&self) {
        self.publish_closing();
        self.cancel.cancel();
        if let Some(handle) = self
            .task
            .lock()
            .expect("coherence task lock poisoned")
            .take()
        {
            handle.abort();
        }
    }
}

/// A cheap, cloneable read handle onto the coherent mempool view.
#[derive(Clone)]
pub struct CoherentSubscriber {
    coherent: Arc<ArcSwap<CoherentSnapshot>>,
    events: broadcast::Sender<Arc<MempoolEvent>>,
    status: NamedAtomicStatus,
    frozen_since: Arc<std::sync::Mutex<Option<Instant>>>,
}

impl std::fmt::Debug for CoherentSubscriber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoherentSubscriber")
            .field("status", &self.status.load())
            .finish_non_exhaustive()
    }
}

impl CoherentSubscriber {
    /// The current coherent view.
    pub fn coherent_snapshot(&self) -> Arc<CoherentSnapshot> {
        self.coherent.load_full()
    }

    /// The coherence-layer status.
    pub fn status(&self) -> StatusType {
        self.status.load()
    }

    /// How long the coherent view has been *continuously* frozen, or `None` when
    /// it is live/serving. A short freeze is normal — coherence freezes on every
    /// block until the set is re-tagged and thaws within a poll — so this is the
    /// signal a caller escalates on: a freeze that outlasts normal thaw means
    /// tip-coherent reads have gone dark and stayed dark (validator unreachable,
    /// NS stuck), which a bare `Frozen` mode cannot distinguish from a transient.
    pub fn frozen_for(&self) -> Option<std::time::Duration> {
        self.frozen_since
            .lock()
            .expect("frozen_since poisoned")
            .map(|since| since.elapsed())
    }

    /// Subscribe to the bounded coherent event stream.
    pub fn subscribe_events(&self) -> broadcast::Receiver<Arc<MempoolEvent>> {
        self.events.subscribe()
    }
}

impl zaino_mempool::ports::TipAwareMempool for CoherentSubscriber {
    fn coherent_snapshot(&self) -> Arc<CoherentSnapshot> {
        self.coherent.load_full()
    }

    fn stream_transactions_until_tip_change(
        &self,
        expected_epoch: Option<NonFinalizedEpoch>,
    ) -> Option<impl futures::Stream<Item = Result<bytes::Bytes, MempoolStreamError>> + Send> {
        // Subscribe before snapshotting so no event between the snapshot load and
        // the subscribe is missed; events at or below `start_sequence` are then
        // discarded as already reflected in the initial snapshot.
        let mut receiver = self.subscribe_events();
        let snapshot = self.coherent_snapshot();

        if let Some(expected) = expected_epoch {
            if !snapshot.is_valid_for_snapshot(expected) {
                return None;
            }
        }

        let start_sequence = snapshot.event_sequence;
        let initial_entries = snapshot.set.entries_in_order().clone();

        // The epoch this stream serves. It closes only when the view becomes live
        // for a *different* epoch — i.e. when the tips re-agree at a *new* tip. It
        // deliberately does NOT close on a transient `Frozen`: the last coherent
        // set stays readable until the new tip is ready, so the caller's next call
        // (with that tip) finds a matching, live view instead of racing Zaino's
        // convergence. A prolonged freeze is bounded by the RPC-layer timeout.
        let stream_epoch = expected_epoch.or(snapshot.valid_for);

        let stream = async_stream::stream! {
            for entry in initial_entries.iter() {
                yield Ok(entry.wire_bytes());
            }

            loop {
                let event = match receiver.recv().await {
                    Ok(event) => event,
                    // Falling behind means transactions were missed. Ending here
                    // without a word would look exactly like the normal
                    // tip-change close, so the consumer would treat a partial
                    // mempool as the whole one.
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        yield Err(MempoolStreamError::Lagged { missed });
                        break;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                };

                match event.as_ref() {
                    MempoolEvent::Added {
                        sequence,
                        valid_for,
                        entry,
                    } => {
                        if *sequence <= start_sequence {
                            continue;
                        }
                        match stream_epoch {
                            Some(epoch) if *valid_for != epoch => break,
                            _ => yield Ok(entry.wire_bytes()),
                        }
                    }
                    MempoolEvent::Live { valid_for, .. } => {
                        // Reconciled to a live set: close only if it is a different
                        // epoch than we opened at (a new tip).
                        match stream_epoch {
                            Some(epoch) if *valid_for != epoch => break,
                            _ => {}
                        }
                    }
                    // Transient: keep serving the last coherent set until the tips
                    // re-agree (handled by `Live`, above). If the epoch cannot be
                    // tracked, fall back to closing on freeze.
                    MempoolEvent::Frozen { .. } => {
                        if stream_epoch.is_none() {
                            break;
                        }
                    }
                    MempoolEvent::Closing { .. } => break,
                }
            }
        };

        Some(stream)
    }
}
