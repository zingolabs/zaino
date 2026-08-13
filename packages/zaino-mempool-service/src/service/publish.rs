//! Snapshot construction and publication: build the next read model from a
//! poll's deltas and store it, or re-stamp / degrade the current one.

use std::collections::HashSet;
use std::sync::Arc;

use zaino_status::StatusType;

use super::state::{admission_key, PollState};
use zaino_mempool::entry::MempoolEntry;
use zaino_mempool::ports::{BlockRef, MempoolPorts};
use zaino_mempool::snapshot::{MempoolCompleteness, MempoolSnapshot};
use zaino_mempool::update::MempoolUpdate;
use zaino_primitives::types::TransactionId;

impl<S: MempoolPorts> super::MempoolService<S> {
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
        // A republish that carries no delta only re-stamps the tag (and possibly
        // the completeness). Reuse the existing collections and, crucially, keep
        // `mempool_generation` — bumping it on an unchanged set would make the
        // coherence layer treat every tip re-tag as new contents and redo its
        // work.
        if added_entries.is_empty() && removed.is_empty() {
            self.publish_retagged(
                current,
                source_tip,
                Self::completeness_for(state),
                state.unadmitted(),
            );
            return;
        }

        let mut final_by_txid = current.by_txid.as_ref().clone();
        // Totals move with the delta rather than being re-summed over the whole
        // set: the set is capped near 13k entries, so re-summing is not dangerous,
        // but it is three needless O(N) passes per poll.
        let mut cost_bytes = current.cost_bytes;
        let mut raw_bytes = current.raw_bytes;
        for txid in &removed {
            if let Some(entry) = final_by_txid.remove(txid) {
                cost_bytes = cost_bytes.saturating_sub(entry.cost());
                raw_bytes = raw_bytes.saturating_sub(entry.raw_len);
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
            raw_bytes += entry.raw_len;
            final_by_txid.insert(entry.txid, Arc::clone(&entry));
            applied_entries.push(entry);
        }

        let completeness = Self::completeness_for(state);

        // Deterministic order, and the one `unique_suffix_match` binary-searches
        // against — see `reversed_txid_key`.
        let mut txids_sorted: Vec<_> = final_by_txid.keys().copied().collect();
        txids_sorted.sort_unstable_by_key(|txid| zaino_mempool::reversed_txid_key(*txid));

        let entries_in_order: Vec<_> = txids_sorted
            .iter()
            .map(|txid| Arc::clone(&final_by_txid[txid]))
            .collect();

        let next_sequence = current.event_sequence.saturating_add(1);
        let next_generation = current.mempool_generation.saturating_add(1);
        let tx_count = final_by_txid.len();

        let snapshot = Arc::new(MempoolSnapshot {
            source_tip,
            mempool_generation: next_generation,
            event_sequence: next_sequence,
            by_txid: Arc::new(final_by_txid),
            txids_sorted: Arc::from(txids_sorted),
            entries_in_order: Arc::from(entries_in_order),
            tx_count,
            raw_bytes,
            cost_bytes,
            completeness,
            unadmitted: Arc::new(state.unadmitted()),
        });

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
        self.current.store(Arc::new(MempoolSnapshot {
            source_tip,
            mempool_generation: current.mempool_generation,
            event_sequence: next_sequence,
            by_txid: Arc::clone(&current.by_txid),
            txids_sorted: Arc::clone(&current.txids_sorted),
            entries_in_order: Arc::clone(&current.entries_in_order),
            tx_count: current.tx_count,
            raw_bytes: current.raw_bytes,
            cost_bytes: current.cost_bytes,
            completeness,
            // Explicitly *not* reused from `current`: a re-tag can carry a fresh
            // shortfall (a poll that deferred its additions publishes through
            // here), and reusing the stale list would report nothing unadmitted.
            unadmitted: Arc::new(unadmitted),
        }));
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

        let next_sequence = current.event_sequence.saturating_add(1);
        let snapshot = Arc::new(MempoolSnapshot {
            source_tip: current.source_tip,
            mempool_generation: current.mempool_generation,
            event_sequence: next_sequence,
            by_txid: Arc::clone(&current.by_txid),
            txids_sorted: Arc::clone(&current.txids_sorted),
            entries_in_order: Arc::clone(&current.entries_in_order),
            tx_count: current.tx_count,
            raw_bytes: current.raw_bytes,
            cost_bytes: current.cost_bytes,
            completeness: MempoolCompleteness::IncompleteSourceError,
            // Carried over: this poll failed before it could recompute the
            // shortfall, so the previous list is the best information there is.
            // It can name a txid that has since left the mempool, which yields a
            // spurious "retry" until the next successful poll — bounded,
            // self-correcting, and the safe direction to be wrong in. Not a leak.
            unadmitted: Arc::clone(&current.unadmitted),
        });

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
