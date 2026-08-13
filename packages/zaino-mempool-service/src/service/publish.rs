//! Snapshot construction and publication: build the next read model from a
//! poll's deltas and store it, or re-stamp / degrade the current one.

use std::collections::HashSet;
use std::sync::Arc;

use zaino_status::StatusType;

use super::state::{admission_key, PollState};
use zaino_mempool::entry::MempoolEntry;
use zaino_mempool::ports::{BlockRef, MempoolSource};
use zaino_mempool::snapshot::{MempoolCompleteness, MempoolSnapshot};
use zaino_mempool::update::MempoolUpdate;
use zaino_primitives::types::TransactionId;

impl<S: MempoolSource> super::MempoolService<S> {
    /// Build and publish the next snapshot from `current` plus the poll's deltas.
    ///
    /// Removals always apply (they only shrink the set). Additions are admitted
    /// in the set's deterministic order for as long as they fit within
    /// `max_cost_bytes`; the rest are refused, remembered in
    /// [`PollState::refused`] so they are not re-fetched every poll, and the set
    /// is marked capacity-limited — the core's DoS backstop.
    pub(super) fn publish_snapshot(
        &self,
        current: &MempoolSnapshot,
        source_tip: Option<BlockRef>,
        mut added_entries: Vec<Arc<MempoolEntry>>,
        removed: Vec<TransactionId>,
        state: &mut PollState,
    ) {
        let completeness = Self::completeness_for(state);

        // Recovery edge: the last published set was short/degraded and this one
        // is whole again. Edge-triggered — emitted only on the transition back
        // to `Complete`, not on every complete publish.
        if current.completeness != MempoolCompleteness::Complete
            && completeness == MempoolCompleteness::Complete
        {
            tracing::info!("mempool recovered: serving a complete set");
        }

        // A republish that carries no delta only re-stamps the tag (and possibly
        // the completeness). Reuse the existing collections and, crucially, keep
        // `mempool_generation` — bumping it on an unchanged set would make the
        // coherence layer treat every tip re-tag as new contents and redo its
        // work.
        if added_entries.is_empty() && removed.is_empty() {
            self.publish_retagged(current, source_tip, completeness, state.unadmitted());
            return;
        }

        let mut final_by_txid = current.by_txid.as_ref().clone();
        // A running cost drives the admission decision below; the published totals
        // are recomputed by `MempoolSnapshot::from_source_set` from the final set,
        // so only the cost needed to decide what fits is tracked here.
        let mut cost_bytes = current.cost_bytes;
        for txid in &removed {
            if let Some(entry) = final_by_txid.remove(txid) {
                cost_bytes = cost_bytes.saturating_sub(entry.cost());
            }
        }

        // Admit on the same validator-assigned, salt-tiebroken key `tick` sliced
        // by, so the exact check here agrees with the estimate that chose what to
        // fetch — and so which transactions make the cut is deterministic without
        // being predictable to their sender (see `admission_key`). *Not* the
        // reversed-txid serving order: that one is grindable.
        let salt = self.admission_salt;
        added_entries.sort_unstable_by_key(|entry| {
            admission_key(salt, entry.entry_time, entry.entry_height, &entry.txid)
        });

        let max_cost_bytes = self.config.max_cost_bytes();
        let mut applied_entries = Vec::with_capacity(added_entries.len());
        for entry in added_entries {
            let cost = entry.cost();
            if cost_bytes.saturating_add(cost) > max_cost_bytes {
                // Remembered with its cost; retried by
                // `retry_refused_that_now_fit` once the set has room again.
                state.refused.insert(entry.txid, cost);
                continue;
            }
            cost_bytes += cost;
            final_by_txid.insert(entry.txid, Arc::clone(&entry));
            applied_entries.push(entry);
        }

        let next_sequence = current.event_sequence.saturating_add(1);
        let next_generation = current.mempool_generation.saturating_add(1);

        // Hand the final set to the constructor, which owns the reversed-key sort
        // and the accounting totals — the read model is consistent by construction.
        let final_entries: Vec<_> = final_by_txid.into_values().collect();
        let snapshot = Arc::new(MempoolSnapshot::from_source_set(
            final_entries,
            source_tip,
            completeness,
            Arc::new(state.unadmitted()),
            next_generation,
            next_sequence,
        ));

        self.current.store(snapshot);

        for txid in removed {
            let _ = self.updates.send(MempoolUpdate::Removed {
                sequence: next_sequence,
                txid,
            });
        }
        for entry in applied_entries {
            let _ = self.updates.send(MempoolUpdate::Added {
                sequence: next_sequence,
                entry,
            });
        }
        // The batch boundary; consumers read `current()` for the coherent whole.
        let _ = self.updates.send(MempoolUpdate::Reset {
            sequence: next_sequence,
        });

        self.status.store(StatusType::Ready);
    }

    /// Re-publish the current set under a new tag / completeness, reusing every
    /// collection and holding `mempool_generation` steady.
    ///
    /// The set itself did not change, so re-cloning the map, re-summing the
    /// totals and re-sorting the txids would all reproduce what is already
    /// there; and a generation bump would falsely tell the coherence layer the
    /// contents moved.
    fn publish_retagged(
        &self,
        current: &MempoolSnapshot,
        source_tip: Option<BlockRef>,
        completeness: MempoolCompleteness,
        unadmitted: HashSet<TransactionId>,
    ) {
        let next_sequence = current.event_sequence.saturating_add(1);
        // `unadmitted` is passed explicitly rather than reused from `current`: a
        // re-tag can carry a fresh shortfall (a poll that deferred its additions
        // publishes through here), and reusing the stale list would report nothing
        // unadmitted.
        self.current.store(Arc::new(current.retagged(
            source_tip,
            completeness,
            Arc::new(unadmitted),
            next_sequence,
        )));
        let _ = self.updates.send(MempoolUpdate::Reset {
            sequence: next_sequence,
        });
        self.status.store(StatusType::Ready);
    }

    /// A source read failed this poll: degrade completeness and reset the
    /// discard run, which this failure interrupted.
    pub(super) fn source_error(&self, state: &mut PollState) {
        state.consecutive_discards = 0;
        self.publish_source_error();
    }

    /// A source read failed this poll: retain the set but mark it incomplete and
    /// re-publish so consumers see the degraded completeness.
    pub(super) fn publish_source_error(&self) {
        let current = self.current.load_full();
        if current.completeness == MempoolCompleteness::IncompleteSourceError {
            self.status.store(StatusType::Syncing);
            return;
        }

        // Non-dedup path: this is the actual transition into the degraded state.
        tracing::warn!("mempool degraded: source unavailable");

        let next_sequence = current.event_sequence.saturating_add(1);
        // `unadmitted` carried over: this poll failed before it could recompute
        // the shortfall, so the previous list is the best information there is. It
        // can name a txid that has since left the mempool, which yields a spurious
        // "retry" until the next successful poll — bounded, self-correcting, and
        // the safe direction to be wrong in. Not a leak.
        let snapshot = Arc::new(current.retagged(
            current.source_tip,
            MempoolCompleteness::IncompleteSourceError,
            Arc::clone(&current.unadmitted),
            next_sequence,
        ));

        self.current.store(snapshot);
        let _ = self.updates.send(MempoolUpdate::Reset {
            sequence: next_sequence,
        });
        self.status.store(StatusType::Syncing);
    }

    pub(super) fn publish_closing(&self) {
        let current = self.current.load_full();
        let next_sequence = current.event_sequence.saturating_add(1);
        let _ = self.updates.send(MempoolUpdate::Closing {
            sequence: next_sequence,
        });
        self.status.store(StatusType::Closing);
    }
}
