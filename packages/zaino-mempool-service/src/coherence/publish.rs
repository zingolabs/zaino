//! Publication: store the coherent snapshot and emit the matching event, whether
//! serving live, freezing, or closing.

use std::sync::Arc;
use std::time::Instant;

use zaino_status::StatusType;

use zaino_mempool::event::MempoolEvent;
use zaino_mempool::ports::{Mempool, NfsEpochObserver, NonFinalizedEpoch};
use zaino_mempool::snapshot::MempoolSnapshot;
use zaino_mempool::tip::{CoherentSnapshot, FreezeReason, MempoolMode, ObservedTips};

impl<M: Mempool, N: NfsEpochObserver> super::CoherenceService<M, N> {
    pub(super) fn publish_live(
        &self,
        prev: &CoherentSnapshot,
        core: Arc<MempoolSnapshot>,
        observed: ObservedTips,
        epoch: NonFinalizedEpoch,
    ) {
        // Serving live: the freeze clock (if any) stops here.
        *self.frozen_since.lock().expect("frozen_since poisoned") = None;

        // Freeze→live edge: emitted only when thawing out of a freeze, not on
        // every live publish.
        if matches!(prev.mode, MempoolMode::Frozen { .. }) {
            tracing::debug!("coherence thawed; serving live");
        }

        // Already live for this epoch at this core generation: nothing to do.
        if prev.is_live_for(epoch)
            && prev.set.mempool_generation == core.mempool_generation
            && prev.observed_tips == observed
        {
            self.status.store(StatusType::Ready);
            return;
        }

        // A steady update at the same epoch: emit an `Added` for each entry newly
        // present since the previous coherent set, so open streams see it live.
        let steady_update = prev.is_live_for(epoch);
        let next_sequence = prev.event_sequence.saturating_add(1);

        let snapshot = Arc::new(CoherentSnapshot {
            set: Arc::clone(&core),
            mode: MempoolMode::Live { valid_for: epoch },
            valid_for: Some(epoch),
            observed_tips: observed,
            event_sequence: next_sequence,
        });
        self.coherent.store(snapshot);

        if steady_update {
            for entry in core.entries_in_order.iter() {
                if !prev.set.by_txid.contains_key(&entry.txid) {
                    let _ = self.events.send(Arc::new(MempoolEvent::Added {
                        sequence: next_sequence,
                        valid_for: epoch,
                        entry: Arc::clone(entry),
                    }));
                }
            }
        }
        let _ = self.events.send(Arc::new(MempoolEvent::Live {
            sequence: next_sequence,
            valid_for: epoch,
        }));

        self.status.store(StatusType::Ready);
    }

    pub(super) fn freeze(
        &self,
        prev: &CoherentSnapshot,
        observed: ObservedTips,
        reason: FreezeReason,
    ) {
        // Start the freeze clock on the transition *into* a freeze, and hold it
        // across repeated freezes (a reason/tip change while still frozen keeps
        // the original start). Cleared only on thaw in `publish_live`.
        if !matches!(prev.mode, MempoolMode::Frozen { .. }) {
            tracing::debug!(reason = ?reason, "coherence froze");
            *self.frozen_since.lock().expect("frozen_since poisoned") = Some(Instant::now());
        }

        // Already frozen for the same reason against the same tips: no re-publish.
        if matches!(prev.mode, MempoolMode::Frozen { reason: r, .. } if r == reason)
            && prev.observed_tips == observed
        {
            self.status.store(StatusType::Syncing);
            return;
        }

        let next_sequence = prev.event_sequence.saturating_add(1);
        let snapshot = Arc::new(CoherentSnapshot {
            // Keep the last blessed set; freezing never mutates it.
            set: Arc::clone(&prev.set),
            mode: MempoolMode::Frozen {
                valid_for: prev.valid_for,
                reason,
            },
            valid_for: prev.valid_for,
            observed_tips: observed,
            event_sequence: next_sequence,
        });
        self.coherent.store(snapshot);
        let _ = self.events.send(Arc::new(MempoolEvent::Frozen {
            sequence: next_sequence,
            reason,
        }));
        self.status.store(StatusType::Syncing);
    }

    pub(super) fn publish_closing(&self) {
        let prev = self.coherent.load_full();
        let next_sequence = prev.event_sequence.saturating_add(1);
        let closing = Arc::new(CoherentSnapshot {
            set: Arc::clone(&prev.set),
            mode: MempoolMode::Closing,
            valid_for: prev.valid_for,
            observed_tips: prev.observed_tips,
            event_sequence: next_sequence,
        });
        self.coherent.store(closing);
        let _ = self.events.send(Arc::new(MempoolEvent::Closing {
            sequence: next_sequence,
        }));
        self.status.store(StatusType::Closing);
    }
}
