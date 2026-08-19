//! Coherent-view publication: serving live, freezing at the last blessed set,
//! and the closing transition — each emitting its matching [`MempoolEvent`].

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
        let was_frozen = matches!(prev.mode, MempoolMode::Frozen { .. });
        *self.frozen_since.lock().expect("frozen_since poisoned") = None;

        if was_frozen {
            // The matching edge to the freeze above, so a `frozen` line is
            // always closed by a `thawed` one and a reader can bound how long
            // coherent reads were withheld. Tested against `prev.mode` rather
            // than the clock: this runs on every live publication, and only the
            // transition is worth a line.
            tracing::debug!(valid_for = ?epoch, "mempool coherence thawed; serving live");
        }

        // Already live for this epoch at this core generation: nothing to do.
        if prev.is_live_for(epoch)
            && prev.set.mempool_generation() == core.mempool_generation()
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
            for entry in core.entries_in_order().iter() {
                if !prev.set.by_txid().contains_key(&entry.txid) {
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
        let was_frozen = matches!(prev.mode, MempoolMode::Frozen { .. });

        // Start the freeze clock on the transition *into* a freeze, and hold it
        // across repeated freezes (a reason/tip change while still frozen keeps
        // the original start). Cleared only on thaw in `publish_live`.
        if !was_frozen {
            *self.frozen_since.lock().expect("frozen_since poisoned") = Some(Instant::now());

            // The edge where tip-coherent reads stop being served. `FreezeReason`
            // is otherwise only a broadcast event, so without this an operator
            // who sees the upstream freeze-escalation warning has no record of
            // *why* it froze — and the reason distinguishes a routine block from
            // a diverged validator.
            //
            // `debug`, not `info`: a freeze is normal — every block causes one —
            // so at the default level this would add a line per block for a
            // perfectly healthy node. The escalation `warn` upstream is what
            // fires when a freeze outlives normal thaw.
            tracing::debug!(?reason, "mempool coherence frozen");
        }

        // Already frozen for the same reason against the same tips: no re-publish.
        if matches!(prev.mode, MempoolMode::Frozen { reason: r, .. } if r == reason)
            && prev.observed_tips == observed
        {
            self.status.store(StatusType::Syncing);
            return;
        }

        if was_frozen {
            // Still frozen, but the cause moved (a tip changed again, or the
            // tips diverged after one went unavailable). Logged separately from
            // the entry edge so a reader can tell a deepening problem from a
            // fresh one — and placed after the dedup guard above, so an
            // unchanged freeze stays silent.
            tracing::debug!(?reason, "mempool coherence freeze reason changed");
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
