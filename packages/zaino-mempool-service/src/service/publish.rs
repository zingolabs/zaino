//! Snapshot publication: building and storing the next [`MempoolSnapshot`],
//! re-tagging an unchanged set, and the source-error / closing degradations.

use std::collections::HashSet;
use std::sync::Arc;

use zaino_status::StatusType;

use zaino_mempool::entry::MempoolEntry;
use zaino_mempool::ports::MempoolSource;
use zaino_mempool::snapshot::{MempoolCompleteness, MempoolSnapshot};
use zaino_mempool::update::MempoolUpdate;
use zaino_primitives::types::{BlockRef, TransactionId};

use super::state::{admission_key, PollState};

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

        let mut final_by_txid = current.by_txid().clone();
        // Tracked across the admission loop below only to decide what fits; the
        // published totals are derived from the final set by `from_entries`, so
        // this running value cannot drift into the snapshot.
        let mut cost_bytes = current.cost_bytes();
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

        Self::log_recovery(current, Self::completeness_for(state));

        // Ordering, ordering-derived collections and totals are all the
        // constructor's job — see `MempoolSnapshot::from_entries`.
        let snapshot = Arc::new(MempoolSnapshot::from_entries(
            current,
            final_by_txid,
            source_tip,
            Self::completeness_for(state),
            state.unadmitted(),
        ));
        let next_sequence = snapshot.event_sequence();

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
        // `unadmitted` is explicitly *not* reused from `current`: a re-tag can
        // carry a fresh shortfall (a poll that deferred its additions publishes
        // through here), and reusing the stale list would report nothing
        // unadmitted.
        Self::log_recovery(current, completeness);
        let snapshot = Arc::new(current.retag(source_tip, completeness, Arc::new(unadmitted)));
        let next_sequence = snapshot.event_sequence();

        self.current.store(snapshot);
        let _ = self.updates.send(MempoolUpdate::Reset {
            sequence: next_sequence,
        });
        self.status.store(StatusType::Ready);
    }

    /// Log the edge back out of source-error degradation.
    ///
    /// Paired with the `warn` in [`publish_source_error`](Self::publish_source_error)
    /// so an operator who sees the mempool go incomplete also sees it come back,
    /// rather than being left to infer recovery from the absence of further
    /// warnings. Edge-triggered on both sides: the poll cadence is sub-second.
    fn log_recovery(previous: &MempoolSnapshot, next: MempoolCompleteness) {
        if previous.completeness() == MempoolCompleteness::IncompleteSourceError
            && next != MempoolCompleteness::IncompleteSourceError
        {
            tracing::info!(
                completeness = ?next,
                "mempool source recovered; polls are being applied again"
            );
        }
    }

    /// A source read failed this poll: degrade completeness and reset the
    /// discard run, which this failure interrupted.
    ///
    /// `cause` names what degraded the set — a validator port, or a condition
    /// like a tip that will not hold still. It is carried here rather than
    /// logged at the call site so the *transition* into degradation reports it:
    /// an operator who sees the mempool go incomplete needs to know why, and
    /// that transition is the only line they get.
    ///
    /// `error` is `None` where the cause is not a single failed call (the
    /// tag-stability backstop) or where the error was already reported with more
    /// context than survives here (the fan-out raw fetch).
    pub(super) fn source_error(
        &self,
        state: &mut PollState,
        cause: &'static str,
        error: Option<&dyn std::fmt::Display>,
    ) {
        state.consecutive_discards = 0;
        self.publish_source_error(cause, error);
    }

    /// Retain the set but mark it incomplete and re-publish, so consumers see
    /// the degraded completeness.
    pub(super) fn publish_source_error(
        &self,
        cause: &'static str,
        error: Option<&dyn std::fmt::Display>,
    ) {
        let current = self.current.load_full();
        let error = error.map(|error| error.to_string());

        if current.completeness() == MempoolCompleteness::IncompleteSourceError {
            // Already degraded. `debug` rather than `warn`: the poll cadence is
            // sub-second, so a validator that stays down would emit thousands of
            // identical warnings. The transition below is the line that matters;
            // this one is for turning the level up to see whether it is still
            // failing, and why.
            tracing::debug!(
                cause,
                error,
                "mempool still degraded; set remains incomplete"
            );
            self.status.store(StatusType::Syncing);
            return;
        }

        tracing::warn!(
            cause,
            error,
            tx_count = current.tx_count(),
            "mempool degraded; serving the last set as incomplete"
        );

        // `unadmitted` is carried over: this poll failed before it could
        // recompute the shortfall, so the previous list is the best information
        // there is. It can name a txid that has since left the mempool, which
        // yields a spurious "retry" until the next successful poll — bounded,
        // self-correcting, and the safe direction to be wrong in. Not a leak.
        let snapshot = Arc::new(current.retag(
            current.source_tip(),
            MempoolCompleteness::IncompleteSourceError,
            Arc::clone(current.unadmitted()),
        ));
        let next_sequence = snapshot.event_sequence();

        self.current.store(snapshot);
        let _ = self.updates.send(MempoolUpdate::Reset {
            sequence: next_sequence,
        });
        self.status.store(StatusType::Syncing);
    }

    pub(super) fn publish_closing(&self) {
        let current = self.current.load_full();
        let next_sequence = current.event_sequence().saturating_add(1);
        let _ = self.updates.send(MempoolUpdate::Closing {
            sequence: next_sequence,
        });
        self.status.store(StatusType::Closing);
    }
}
