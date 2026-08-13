//! The single writer task: the poll loop, the per-tick diff/admit decision, and
//! the reconcile-decision helpers that classify what each poll should publish.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use futures::{stream, StreamExt as _};
use zaino_status::StatusType;

use zaino_mempool::entry::MempoolEntry;
use zaino_mempool::ports::MempoolSource;
use zaino_mempool::snapshot::{MempoolCompleteness, MempoolSnapshot};
use zaino_primitives::types::BlockRef;
use zaino_source::{GetRawMempoolTransactionError, MempoolTxMeta, QueryError};

use super::state::{admission_key, PollState};

/// Fraction of `max_cost_bytes` the set must fall back below before previously
/// refused transactions are retried at all. Hysteresis: retrying the instant a
/// single byte frees up would re-fetch refused transactions on every poll.
const CAPACITY_LOW_WATER_PERCENT: u64 = 90;

/// Consecutive polls the tag-stability guard may discard before the set is
/// republished as incomplete.
///
/// Each discard means a block landed mid-poll, which is normal once in a while.
/// A long run of them means the set is not converging (a block burst, regtest
/// mining, a slow link), and consumers are better served by being told the view
/// is stale than by an unchanging `Complete` set.
///
/// `pub(super)` only so [`PollState::consecutive_discards`](super::state::PollState)
/// can name it in its doc link; the value is used only within this module.
pub(super) const MAX_CONSECUTIVE_DISCARDS: u32 = 5;

impl<S: MempoolSource> super::MempoolService<S> {
    /// The single writer task.
    ///
    /// Instrumented as one long-lived span rather than per tick: at a sub-second
    /// cadence a span per poll would dominate any trace it appeared in, and the
    /// interesting events (degradation, recovery) are edges the loop reports
    /// itself.
    #[tracing::instrument(
        name = "mempool_poll_loop",
        skip_all,
        fields(poll_interval_ms = self.config.poll_interval().as_millis()),
    )]
    pub(super) async fn run(self: Arc<Self>) {
        self.status.store(StatusType::Syncing);

        let mut interval = tokio::time::interval(self.config.poll_interval());
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut block_wake = self.source.subscribe_to_blocks_received();
        let mut state = PollState::default();

        tracing::debug!(
            block_wake = block_wake.is_some(),
            "mempool poll loop started"
        );

        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    tracing::debug!("mempool poll loop cancelled; publishing Closing");
                    self.publish_closing();
                    return;
                }
                _ = interval.tick() => {
                    self.tick(&mut state).await;
                }
                _ = async {
                    match block_wake.as_mut() {
                        Some(rx) => {
                            let _ = rx.changed().await;
                        }
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    self.tick(&mut state).await;
                }
            }
        }
    }

    async fn tick(&self, state: &mut PollState) {
        // Tag: the validator tip this poll's fetch window opens at.
        //
        // Read fresh every poll, never carried over from the last one: this read
        // is also how a tip *change* is detected over an otherwise unchanged
        // mempool, which the coherence layer depends on to thaw.
        let tip_before = match self.source.get_mempool_source_tip().await {
            Ok(tip) => BlockRef::from_tip(tip),
            Err(error) => return self.source_error(state, "get_mempool_source_tip", Some(&error)),
        };

        let txids = match self.source.get_mempool_txids().await {
            Ok(txids) => txids,
            // The source errored: keep the last set, mark it incomplete, and
            // retry next poll. The core never freezes.
            Err(error) => return self.source_error(state, "get_mempool_txids", Some(&error)),
        };

        let current = self.current.load_full();
        let source_txids: HashSet<_> = txids.iter().copied().collect();

        let removed: Vec<_> = current
            .by_txid()
            .keys()
            .filter(|txid| !source_txids.contains(*txid))
            .copied()
            .collect();

        // A refused transaction that has left the source's mempool is no longer
        // ours to admit, so it leaves the memo with it.
        state.refused.retain(|txid, _| source_txids.contains(txid));
        self.retry_refused_that_now_fit(&current, state);

        // Deferral is recomputed each poll: a txid deferred last poll is
        // rediscovered by this diff and admitted as soon as the listing is due.
        state.deferred.clear();

        let added_txids: Vec<_> = txids
            .into_iter()
            .filter(|txid| {
                !current.by_txid().contains_key(txid) && !state.refused.contains_key(txid)
            })
            .collect();

        if added_txids.is_empty() && removed.is_empty() {
            // The set is unchanged; re-publish only if what we would stamp on it
            // moved.
            self.republish_if_changed(&current, tip_before, state).await;
            return;
        }

        // Everything from here is about admitting additions. The capacity bound
        // is applied to what we *fetch*, not only to what we retain: fetching
        // first and refusing afterwards would perform the whole memory blow-up
        // the bound exists to prevent.
        let mut added_meta: Vec<MempoolTxMeta> = Vec::new();

        if !added_txids.is_empty() {
            // How many additions can possibly fit, assuming every one costs the
            // ZIP-401 floor. An entry can only cost *more*, so this over-counts
            // and the exact check in `publish_snapshot` still decides.
            let max_cost_bytes = self.config.max_cost_bytes();
            let headroom = max_cost_bytes.saturating_sub(current.cost_bytes());
            // Saturating rather than `as`: on a 32-bit target the quotient can
            // exceed `usize::MAX`, and a truncating cast is exactly wrong at the
            // boundary — it wraps to a *small* number, and a wrap to zero would
            // take the refuse-everything branch below while the set had room.
            // Saturating keeps the over-count in the direction the estimate is
            // already deliberately wrong in.
            let max_admissible = usize::try_from(
                headroom / zaino_mempool::config::MEMPOOL_TRANSACTION_COST_THRESHOLD,
            )
            .unwrap_or(usize::MAX);

            if max_admissible == 0 {
                // The set is already at the bound. Refuse without fetching, and
                // in particular without paying for the whole-mempool metadata
                // walk to admit nothing.
                for txid in added_txids {
                    state.refused.insert(
                        txid,
                        zaino_mempool::config::MEMPOOL_TRANSACTION_COST_THRESHOLD,
                    );
                }
            } else {
                // The listing is heavy on the source (a whole-mempool walk), so
                // it is floored at `metadata_min_interval`. Additions cannot be
                // admitted without it — but the rest of this poll can still be
                // published, so only the additions are deferred.
                if !self.metadata_fetch_is_due(state) {
                    state.deferred = added_txids.into_iter().collect();
                } else {
                    let metadata = match self.source.get_mempool_metadata().await {
                        Ok(metadata) => metadata,
                        Err(error) => {
                            return self.source_error(state, "get_mempool_metadata", Some(&error))
                        }
                    };
                    state.last_metadata_fetch = Some(Instant::now());
                    let meta_by_txid: HashMap<_, _> =
                        metadata.into_iter().map(|meta| (meta.txid, meta)).collect();
                    // A txid in the light list but absent from verbose raced away
                    // between the two calls; skip it (the raw fetch would too).
                    added_meta = added_txids
                        .into_iter()
                        .filter_map(|txid| meta_by_txid.get(&txid).copied())
                        .collect();

                    // Admission order comes from validator-assigned metadata, so
                    // it cannot be ground by a sender (see `admission_key`). This
                    // is why the slice happens *after* the metadata fetch: the
                    // ungrindable key does not exist before it.
                    let salt = self.admission_salt;
                    added_meta.sort_unstable_by_key(|meta| {
                        admission_key(salt, meta.entry_time, meta.entry_height, &meta.txid)
                    });

                    // Refuse the tail without fetching it. The memo records the
                    // floor cost rather than the true one, which only makes
                    // `retry_refused_that_now_fit` conservative for these — it
                    // can under-admit, never re-refuse in a loop. An entry whose
                    // true cost exceeds the floor is re-refused with its real
                    // cost on the retry that fetches it, so the memo
                    // self-corrects after one wasted fetch.
                    for meta in added_meta.split_off(max_admissible.min(added_meta.len())) {
                        state.refused.insert(
                            meta.txid,
                            zaino_mempool::config::MEMPOOL_TRANSACTION_COST_THRESHOLD,
                        );
                    }
                }
            }
        }

        // A poll whose additions were all deferred or refused still has a
        // removals-and-retag half worth publishing: coherence thaws on the
        // re-tag, so withholding it would make `metadata_min_interval` extend
        // the post-block freeze by its own length.
        if added_meta.is_empty() && removed.is_empty() {
            self.republish_if_changed(&current, tip_before, state).await;
            return;
        }

        let next_generation = current.mempool_generation().saturating_add(1);
        let added_entries = match self
            .fetch_added_entries_bounded(added_meta, next_generation)
            .await
        {
            Some(entries) => entries,
            None => return self.source_error(state, "get_raw_mempool_transaction", None),
        };

        // Tag-stability guard: the validator tip must not have moved across the
        // whole fetch window. If it did, this poll's data is smeared across two
        // tips and cannot be soundly tagged with either — discard and retry. This
        // is what makes `source_tip` a single-source pair with the set, so the
        // coherence layer can trust `V == NS` without re-fetching.
        if !self.tip_is_stable(tip_before, state).await {
            return;
        }

        self.publish_snapshot(&current, Some(tip_before), added_entries, removed, state);
    }

    /// Re-stamp the current set when only the tag, completeness or unadmitted
    /// list moved, so the coherence layer still learns of a validator-tip
    /// advance over a steady mempool.
    ///
    /// Comparing against what we *would* publish (rather than against
    /// `Complete`) is what keeps a steadily capacity-limited or
    /// metadata-deferring mempool from republishing on every single poll: while
    /// the short set stays the same short set, nothing is stamped and nothing
    /// wakes the coherence layer.
    async fn republish_if_changed(
        &self,
        current: &MempoolSnapshot,
        tip_before: BlockRef,
        state: &mut PollState,
    ) {
        let next_unadmitted = state.unadmitted();
        if current.source_tip() != Some(tip_before)
            || current.completeness() != Self::completeness_for(state)
            || **current.unadmitted() != next_unadmitted
        {
            // Only once the tag is confirmed stable — never smeared across a
            // mid-poll block.
            if self.tip_is_stable(tip_before, state).await {
                self.publish_snapshot(current, Some(tip_before), Vec::new(), Vec::new(), state);
            }
        } else {
            self.status.store(StatusType::Ready);
        }
    }

    /// Whether the validator tip is still `tip_before` — i.e. no block arrived
    /// mid-poll to smear the set across two tips.
    ///
    /// Counts discards in `state`: after [`MAX_CONSECUTIVE_DISCARDS`] in a row the set is
    /// republished as [`IncompleteSourceError`](MempoolCompleteness::IncompleteSourceError),
    /// so consumers are told the mempool is not converging instead of silently
    /// serving an increasingly stale set.
    async fn tip_is_stable(&self, tip_before: BlockRef, state: &mut PollState) -> bool {
        let stable = matches!(
            self.source.get_mempool_source_tip().await,
            Ok(tip) if BlockRef::from_tip(tip) == tip_before
        );
        if stable {
            state.consecutive_discards = 0;
        } else {
            state.consecutive_discards = state.consecutive_discards.saturating_add(1);
            if state.consecutive_discards >= MAX_CONSECUTIVE_DISCARDS {
                // Not a source failure: the validator is answering fine, the tip
                // simply will not hold still long enough to tag a set against.
                // Named separately so an operator is not sent looking for a
                // broken connection.
                tracing::warn!(
                    consecutive_discards = state.consecutive_discards,
                    "mempool tip moved across every recent poll window; set is not converging"
                );
                self.publish_source_error("tip_unstable", None);
            }
        }
        stable
    }

    /// Whether enough time has passed since the last metadata listing.
    fn metadata_fetch_is_due(&self, state: &PollState) -> bool {
        match state.last_metadata_fetch {
            Some(last) => last.elapsed() >= self.config.metadata_min_interval(),
            None => true,
        }
    }

    /// Drop refused transactions from the memo once the set can take them again,
    /// so the next poll rediscovers and admits them.
    ///
    /// Two conditions, both needed: the set must have fallen below the low-water
    /// mark (hysteresis, so a mempool sitting at the bound does not retry every
    /// poll), and the individual transaction must fit in the remaining headroom
    /// (exactness, so a retry cannot end in an immediate re-refusal).
    fn retry_refused_that_now_fit(&self, current: &MempoolSnapshot, state: &mut PollState) {
        if state.refused.is_empty() {
            return;
        }
        let max_cost_bytes = self.config.max_cost_bytes();
        // Multiply first: `max / 100 * pct` truncates to zero for any bound
        // below 100, which would make `cost_bytes >= 0` always true and strand
        // every refusal forever.
        if current.cost_bytes() >= max_cost_bytes.saturating_mul(CAPACITY_LOW_WATER_PERCENT) / 100 {
            return;
        }
        let headroom = max_cost_bytes.saturating_sub(current.cost_bytes());
        state.refused.retain(|_, cost| *cost > headroom);
    }

    /// The completeness the next published set carries.
    ///
    /// Both classes below are *short*, not *wrong* — the set accurately holds
    /// what it holds — but they are reported separately so operator telemetry
    /// attributes the shortfall to the right cause. The capacity bound wins when
    /// both apply: it is the more serious of the two, and it does not clear on
    /// its own the way a deferral does.
    pub(super) fn completeness_for(state: &PollState) -> MempoolCompleteness {
        if !state.refused.is_empty() {
            MempoolCompleteness::IncompleteCapacityLimited
        } else if !state.deferred.is_empty() {
            MempoolCompleteness::IncompletePendingMetadata
        } else {
            MempoolCompleteness::Complete
        }
    }

    async fn fetch_added_entries_bounded(
        &self,
        added: Vec<MempoolTxMeta>,
        next_generation: u64,
    ) -> Option<Vec<Arc<MempoolEntry>>> {
        let concurrency = self.config.max_concurrent_raw_fetches();
        let source = &self.source;

        let results = stream::iter(added)
            .map(|meta| async move {
                let serialized_tx = match source.get_raw_mempool_transaction(meta.txid).await {
                    Ok(serialized_tx) => serialized_tx,
                    // The normal race: the transaction left the mempool between
                    // listing and fetch. Skip it, don't fail the poll.
                    //
                    // Only this *modelled* answer means "gone". Treating a
                    // transport failure as gone too would let any validator
                    // hiccup silently delete a transaction from a set that still
                    // publishes as complete.
                    Err(QueryError::Domain(GetRawMempoolTransactionError::NotFound(_))) => {
                        return Ok(None)
                    }
                    Err(e) => return Err(zaino_mempool::MempoolError::source(e)),
                };

                let raw_len = serialized_tx.len() as u64;
                Ok(Some(Arc::new(MempoolEntry {
                    txid: meta.txid,
                    // One copy out of the validator's response, shared from here
                    // on (see `MempoolEntry::serialized_tx`).
                    serialized_tx: bytes::Bytes::from(serialized_tx),
                    raw_len,
                    entry_height: meta.entry_height,
                    entry_time: meta.entry_time,
                    first_seen_generation: next_generation,
                })))
            })
            .buffer_unordered(concurrency)
            .collect::<Vec<Result<Option<Arc<MempoolEntry>>, zaino_mempool::MempoolError>>>()
            .await;

        let mut entries = Vec::new();
        for result in results {
            match result {
                Ok(Some(entry)) => entries.push(entry),
                Ok(None) => {}
                // Any raw-fetch error aborts this poll's update; the last set
                // stays readable and the next poll retries. The error was fully
                // built before this point, so dropping it unlogged would discard
                // the only account of why an addition never appeared.
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "raw mempool transaction fetch failed; abandoning this poll's additions"
                    );
                    return None;
                }
            }
        }
        Some(entries)
    }
}
