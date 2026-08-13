//! The core mempool service: a single writer task that mirrors the validator's
//! mempool as an immutable, bounded, **tip-agnostic** read model.
//!
//! # Tip-agnostic, but tip-tagged
//!
//! This service never freezes. Each poll it diffs the validator's current mempool
//! against the held set and applies the delta, so tip-agnostic reads
//! (`GetMempoolTx`, `getrawmempool`, `getmempoolinfo`) always serve the live set
//! — even mid-reorg. It does, however, **tag** every published snapshot with the
//! validator tip it was fetched at ([`MempoolSnapshot::source_tip`]), read from the
//! *same* source that serves the mempool data. That single-source tag is what lets
//! the optional coherence layer (`crate::coherence`) decide `V == NS` without
//! re-fetching; without it, coherence would have to correlate the set and the tip
//! across two independent reads — the race the rework closed.
//!
//! The only bound the core enforces itself is the ZIP-401 capacity backstop:
//! additions are admitted in the set's deterministic order up to
//! `max_cost_bytes`; those that would breach it are refused, remembered, and the
//! set is marked
//! [`IncompleteCapacityLimited`](MempoolCompleteness::IncompleteCapacityLimited)
//! rather than exceeding the bound.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasher as _, Hash as _, Hasher as _};
use std::sync::Arc;
use std::time::Instant;

use arc_swap::ArcSwap;
use futures::{stream, StreamExt as _};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use zaino_status::{NamedAtomicStatus, StatusType};

use crate::subscriber::MempoolSubscriber;
use zaino_mempool::config::MempoolConfig;
use zaino_mempool::entry::MempoolEntry;
use zaino_mempool::ports::MempoolSource;
use zaino_mempool::snapshot::{MempoolCompleteness, MempoolSnapshot};
use zaino_mempool::update::MempoolUpdate;
use zaino_primitives::types::{BlockRef, Height, TransactionId};
use zaino_source::{GetRawMempoolTransactionError, MempoolTxMeta, QueryError};

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
const MAX_CONSECUTIVE_DISCARDS: u32 = 5;

/// The order additions are admitted in when the set is at capacity.
///
/// Keyed on validator-assigned metadata first — arrival time, then tip-at-entry
/// height — so a sender cannot buy priority. The txid only breaks ties, and only
/// through a per-process salt, because the timestamp is whole-second granular:
/// without the salt every transaction arriving in the same second would be
/// ordered by raw txid bytes, which the sender *can* grind.
///
/// The honest claim is that admission is **unpredictable to the sender**, not
/// that it is globally fair. An attacker flooding at capacity still lands in the
/// same one-second bucket as the transactions they displace; the salt reduces
/// that from "always wins" to "wins only by luck".
///
/// `entry_time: None` sorts last: an entry the source gave no timestamp for has
/// no claim to priority over one it did.
fn admission_key(
    salt: u64,
    entry_time: Option<i64>,
    entry_height: Height,
    txid: &TransactionId,
) -> (bool, i64, u32, u64) {
    let mut hasher = DefaultHasher::new();
    salt.hash(&mut hasher);
    <[u8; 32]>::from(*txid).hash(&mut hasher);
    (
        // `false` sorts before `true`, so "has a timestamp" comes first.
        entry_time.is_none(),
        entry_time.unwrap_or(i64::MAX),
        u32::from(entry_height),
        hasher.finish(),
    )
}

/// State owned by the single writer task across polls.
///
/// Threaded through `&mut` rather than held behind a lock precisely because
/// there is exactly one writer: the poll loop.
#[derive(Default)]
struct PollState {
    /// When the metadata listing was last fetched, for the
    /// [`metadata_min_interval`](MempoolConfig::metadata_min_interval) floor.
    last_metadata_fetch: Option<Instant>,

    /// Transactions the capacity backstop refused, and what each would cost.
    ///
    /// Without this memo they would be rediscovered by the very next diff (which
    /// is recomputed from the held set) and re-fetched forever, hammering the
    /// source while the set stayed capacity-limited. Entries leave the memo when
    /// they leave the source's mempool, or when the set has both fallen below
    /// the low-water mark and freed enough room for that specific transaction —
    /// keeping the cost is what makes the retry decision exact instead of a
    /// guess that can re-refuse in a loop.
    refused: HashMap<TransactionId, u64>,

    /// Polls discarded in a row by the tag-stability guard, for the
    /// [`MAX_CONSECUTIVE_DISCARDS`] backstop.
    consecutive_discards: u32,

    /// Txids this poll saw in the source's listing but did not admit because the
    /// metadata listing was deferred by `metadata_min_interval`.
    ///
    /// Distinct from [`refused`](Self::refused): these are not over the capacity
    /// bound, only waiting for their metadata. Both feed
    /// [`MempoolSnapshot::unadmitted`].
    deferred: HashSet<TransactionId>,
}

impl PollState {
    /// Every txid the source reported that is not in the published set: refused
    /// by the capacity bound, or deferred awaiting metadata.
    ///
    /// Bounded by the txid-listing cap. Consumers use it to tell "Zaino is short
    /// this transaction, ask again" from "this transaction does not exist".
    fn unadmitted(&self) -> HashSet<TransactionId> {
        self.refused
            .keys()
            .copied()
            .chain(self.deferred.iter().copied())
            .collect()
    }
}

/// The core mempool read-model service.
///
/// Generic over its one outbound port ([`MempoolSource`]) so the core has no
/// `zaino-state` dependency and no chain-tip knowledge beyond the tag it stamps.
pub struct MempoolService<S: MempoolSource> {
    source: S,
    current: Arc<ArcSwap<MempoolSnapshot>>,
    updates: broadcast::Sender<MempoolUpdate>,
    config: MempoolConfig,
    status: NamedAtomicStatus,
    cancel: CancellationToken,
    task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Per-process salt for the admission tiebreak (see [`admission_key`]).
    admission_salt: u64,
}

impl<S: MempoolSource> std::fmt::Debug for MempoolService<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MempoolService")
            .field("status", &self.status.load())
            .finish_non_exhaustive()
    }
}

impl<S: MempoolSource> MempoolService<S> {
    /// Spawn the core service and its background poll task.
    ///
    /// The admission tiebreak salt is drawn per process, so the order additions
    /// take at capacity is not predictable to a transaction's sender (see the
    /// `admission_key` tiebreak).
    pub fn spawn(source: S, config: MempoolConfig, cancel: CancellationToken) -> Arc<Self> {
        // `RandomState` is the std hasher's per-process random seed; hashing a
        // fixed value through it yields a stable random salt without pulling in
        // an RNG dependency.
        let salt = std::collections::hash_map::RandomState::new().hash_one(0u8);
        Self::spawn_with_admission_salt(source, config, cancel, salt)
    }

    /// [`Self::spawn`] with an explicit admission salt.
    ///
    /// Exists so tests can pin the tiebreak and assert its *properties*
    /// deterministically (same salt ⇒ same order, different salt ⇒ different
    /// order) rather than by sampling a random one.
    pub fn spawn_with_admission_salt(
        source: S,
        config: MempoolConfig,
        cancel: CancellationToken,
        admission_salt: u64,
    ) -> Arc<Self> {
        let (updates, _) = broadcast::channel(config.event_buffer_len());

        let service = Arc::new(Self {
            source,
            current: Arc::new(ArcSwap::from_pointee(MempoolSnapshot::empty())),
            updates,
            config,
            status: NamedAtomicStatus::new("Mempool", StatusType::Spawning),
            cancel,
            task: std::sync::Mutex::new(None),
            admission_salt,
        });

        let task_service = Arc::clone(&service);
        let handle = tokio::spawn(async move {
            task_service.run().await;
        });

        *service.task.lock().expect("mempool task lock poisoned") = Some(handle);

        service
    }

    /// A cheap, cloneable read handle onto the core mempool.
    pub fn subscriber(&self) -> MempoolSubscriber {
        MempoolSubscriber::new(
            Arc::clone(&self.current),
            self.updates.clone(),
            self.config.clone(),
            self.status.clone(),
        )
    }

    /// Current service status.
    pub fn status(&self) -> StatusType {
        self.status.load()
    }

    /// The current mempool memory bound (max total ZIP-401 cost), in bytes.
    pub fn max_cost_bytes(&self) -> u64 {
        self.config.max_cost_bytes()
    }

    /// Adjust the mempool memory bound at runtime; takes effect on the next
    /// poll. Lowering it below the current set does not evict — the set shrinks
    /// as transactions are mined, and additions are refused meanwhile.
    ///
    /// Deliberately on the service, not on the read handles: it is a
    /// capacity-control knob for whoever owns the mempool, and reaching it
    /// through a cloneable read handle would make every RPC path a freeze
    /// switch.
    pub fn set_max_cost_bytes(&self, bytes: u64) {
        self.config.set_max_cost_bytes(bytes);
    }

    /// Signal shutdown: publish the `Closing` update, then stop the task.
    ///
    /// `Closing` is published synchronously here (rather than relying on the
    /// background task to observe cancellation first) so subscribers reliably see
    /// it even though the task is aborted immediately after.
    pub fn close(&self) {
        self.publish_closing();
        self.cancel.cancel();
        if let Some(handle) = self.task.lock().expect("mempool task lock poisoned").take() {
            handle.abort();
        }
    }

    // ---- background task ------------------------------------------------

    async fn run(self: Arc<Self>) {
        self.status.store(StatusType::Syncing);

        let mut interval = tokio::time::interval(self.config.poll_interval());
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut block_wake = self.source.subscribe_to_blocks_received();
        let mut state = PollState::default();

        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => {
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
            Err(_) => return self.source_error(state),
        };

        let txids = match self.source.get_mempool_txids().await {
            Ok(txids) => txids,
            // The source errored: keep the last set, mark it incomplete, and
            // retry next poll. The core never freezes.
            Err(_) => return self.source_error(state),
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
                        Err(_) => return self.source_error(state),
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
            None => return self.source_error(state),
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
                self.publish_source_error();
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
    fn completeness_for(state: &PollState) -> MempoolCompleteness {
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
                // stays readable and the next poll retries.
                Err(_) => return None,
            }
        }
        Some(entries)
    }

    // ---- publication ----------------------------------------------------

    /// Build and publish the next snapshot from `current` plus the poll's deltas.
    ///
    /// Removals always apply (they only shrink the set). Additions are admitted
    /// in the set's deterministic order for as long as they fit within
    /// `max_cost_bytes`; the rest are refused, remembered in
    /// [`PollState::refused`] so they are not re-fetched every poll, and the set
    /// is marked capacity-limited — the core's DoS backstop.
    fn publish_snapshot(
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
        let snapshot = Arc::new(current.retag(source_tip, completeness, Arc::new(unadmitted)));
        let next_sequence = snapshot.event_sequence();

        self.current.store(snapshot);
        let _ = self.updates.send(MempoolUpdate::Reset {
            sequence: next_sequence,
        });
        self.status.store(StatusType::Ready);
    }

    /// A source read failed this poll: degrade completeness and reset the
    /// discard run, which this failure interrupted.
    fn source_error(&self, state: &mut PollState) {
        state.consecutive_discards = 0;
        self.publish_source_error();
    }

    /// A source read failed this poll: retain the set but mark it incomplete and
    /// re-publish so consumers see the degraded completeness.
    fn publish_source_error(&self) {
        let current = self.current.load_full();
        if current.completeness() == MempoolCompleteness::IncompleteSourceError {
            self.status.store(StatusType::Syncing);
            return;
        }

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

    fn publish_closing(&self) {
        let current = self.current.load_full();
        let next_sequence = current.event_sequence().saturating_add(1);
        let _ = self.updates.send(MempoolUpdate::Closing {
            sequence: next_sequence,
        });
        self.status.store(StatusType::Closing);
    }
}
