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
//! additions that would breach `max_cost_bytes` are dropped and the set is marked
//! [`IncompleteCapacityLimited`](MempoolCompleteness::IncompleteCapacityLimited)
//! rather than exceeding the bound.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use arc_swap::ArcSwap;
use futures::{stream, StreamExt as _};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use zaino_common::status::{NamedAtomicStatus, StatusType};

use crate::subscriber::MempoolSubscriber;
use zaino_mempool::config::MempoolConfig;
use zaino_mempool::entry::MempoolEntry;
use zaino_mempool::ports::{BlockRef, MempoolSource, MempoolTxMeta};
use zaino_mempool::snapshot::{MempoolCompleteness, MempoolSnapshot};
use zaino_mempool::update::MempoolUpdate;

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
    pub fn spawn(source: S, config: MempoolConfig, cancel: CancellationToken) -> Arc<Self> {
        let (updates, _) = broadcast::channel(config.event_buffer_len);

        let service = Arc::new(Self {
            source,
            current: Arc::new(ArcSwap::from_pointee(MempoolSnapshot::empty_not_ready())),
            updates,
            config,
            status: NamedAtomicStatus::new("Mempool", StatusType::Spawning),
            cancel,
            task: std::sync::Mutex::new(None),
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

        let mut interval = tokio::time::interval(self.config.poll_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut block_wake = self.source.subscribe_to_blocks_received();

        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    self.publish_closing();
                    return;
                }
                _ = interval.tick() => {
                    self.tick().await;
                }
                _ = async {
                    match block_wake.as_mut() {
                        Some(rx) => {
                            let _ = rx.changed().await;
                        }
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    self.tick().await;
                }
            }
        }
    }

    async fn tick(&self) {
        // Tag: the validator tip at the start of this poll's fetch window.
        let tip_before = match self.source.get_mempool_source_tip().await {
            Ok(tip) => tip,
            Err(_) => return self.publish_source_error(),
        };

        let txids = match self.source.get_mempool_txids().await {
            Ok(Some(txids)) => txids,
            // Source could not answer, or errored: keep the last set, mark it
            // incomplete, and retry next poll. The core never freezes.
            Ok(None) | Err(_) => return self.publish_source_error(),
        };

        let current = self.current.load_full();
        let source_txids: HashSet<_> = txids.iter().copied().collect();

        let removed: Vec<_> = current
            .by_txid
            .keys()
            .filter(|txid| !source_txids.contains(*txid))
            .copied()
            .collect();

        let added_txids: Vec<_> = txids
            .into_iter()
            .filter(|txid| !current.by_txid.contains_key(txid))
            .collect();

        if added_txids.is_empty() && removed.is_empty() {
            // The set is unchanged. Re-publish only if the tag or completeness
            // moved, so the coherence layer still learns of a validator-tip
            // advance over a steady mempool — but only once the tag is confirmed
            // stable (below), never smeared across a mid-poll block.
            if current.source_tip != tip_before
                || current.completeness != MempoolCompleteness::Complete
            {
                if self.tip_is_stable(tip_before).await {
                    self.publish_snapshot(&current, tip_before, Vec::new(), Vec::new());
                }
            } else {
                self.status.store(StatusType::Ready);
            }
            return;
        }

        // Heights for new transactions come from the verbose listing, fetched only
        // now (and only because there are additions).
        let added_meta: Vec<MempoolTxMeta> = if added_txids.is_empty() {
            Vec::new()
        } else {
            let metadata = match self.source.get_mempool_metadata().await {
                Ok(Some(metadata)) => metadata,
                Ok(None) | Err(_) => return self.publish_source_error(),
            };
            let meta_by_txid: HashMap<_, _> =
                metadata.into_iter().map(|meta| (meta.txid, meta)).collect();
            // A txid in the light list but absent from verbose raced away between
            // the two calls; skip it (the raw fetch would skip it too).
            added_txids
                .into_iter()
                .filter_map(|txid| meta_by_txid.get(&txid).copied())
                .collect()
        };

        let next_generation = current.mempool_generation.saturating_add(1);
        let added_entries = match self
            .fetch_added_entries_bounded(added_meta, next_generation)
            .await
        {
            Some(entries) => entries,
            None => return self.publish_source_error(),
        };

        // Tag-stability guard: the validator tip must not have moved across the
        // whole fetch window. If it did, this poll's data is smeared across two
        // tips and cannot be soundly tagged with either — discard and retry. This
        // is what makes `source_tip` a single-source pair with the set, so the
        // coherence layer can trust `V == NS` without re-fetching.
        if !self.tip_is_stable(tip_before).await {
            return;
        }

        self.publish_snapshot(&current, tip_before, added_entries, removed);
    }

    /// Whether the validator tip is still `tip_before` after the fetch window —
    /// i.e. no block arrived mid-poll to smear the set across two tips.
    async fn tip_is_stable(&self, tip_before: Option<BlockRef>) -> bool {
        matches!(self.source.get_mempool_source_tip().await, Ok(tip) if tip == tip_before)
    }

    async fn fetch_added_entries_bounded(
        &self,
        added: Vec<MempoolTxMeta>,
        next_generation: u64,
    ) -> Option<Vec<Arc<MempoolEntry>>> {
        let concurrency = self.config.max_concurrent_raw_fetches.max(1);
        let source = &self.source;

        let results = stream::iter(added)
            .map(|meta| async move {
                let raw = source.get_raw_mempool_transaction(meta.txid).await?;

                // A `None` here is the normal race: the tx left the mempool
                // between listing and fetch. Skip it, don't fail the poll.
                let Some(serialized_tx) = raw else {
                    return Ok(None);
                };

                let raw_len = serialized_tx.as_ref().len() as u32;
                Ok(Some(Arc::new(MempoolEntry {
                    txid: meta.txid,
                    serialized_tx: Arc::new(serialized_tx),
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
    /// Removals always apply (they only shrink the set). Additions apply only if
    /// they keep the set within `max_cost_bytes`; otherwise they are dropped and
    /// the set is marked capacity-limited — the core's DoS backstop.
    fn publish_snapshot(
        &self,
        current: &MempoolSnapshot,
        source_tip: Option<BlockRef>,
        added_entries: Vec<Arc<MempoolEntry>>,
        removed: Vec<zebra_chain::transaction::Hash>,
    ) {
        let mut next_by_txid = current.by_txid.as_ref().clone();
        for txid in &removed {
            next_by_txid.remove(txid);
        }

        // Tentatively add, then check the bound.
        let mut tentative = next_by_txid.clone();
        for entry in &added_entries {
            tentative.insert(entry.txid, Arc::clone(entry));
        }
        let tentative_cost: u64 = tentative.values().map(|entry| entry.cost()).sum();

        let (final_by_txid, applied_entries, completeness) =
            if tentative_cost > self.config.max_cost_bytes() && !added_entries.is_empty() {
                // Applying the additions would breach the DoS backstop: keep the
                // removals, drop the additions, and mark the set incomplete.
                (
                    next_by_txid,
                    Vec::new(),
                    MempoolCompleteness::IncompleteCapacityLimited,
                )
            } else {
                (tentative, added_entries, MempoolCompleteness::Complete)
            };

        let cost_bytes: u64 = final_by_txid.values().map(|entry| entry.cost()).sum();
        let raw_bytes: u64 = final_by_txid
            .values()
            .map(|entry| entry.raw_len as u64)
            .sum();

        // Sort by *reversed* txid bytes: deterministic order that makes the
        // lightwallet exclude filter (which matches on txid suffixes) a
        // binary-searchable prefix match over the reversed bytes.
        let mut txids_sorted: Vec<_> = final_by_txid.keys().copied().collect();
        txids_sorted.sort_unstable_by(|a, b| a.0.iter().rev().cmp(b.0.iter().rev()));

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

    /// A source read failed this poll: retain the set but mark it incomplete and
    /// re-publish so consumers see the degraded completeness.
    fn publish_source_error(&self) {
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
        });

        self.current.store(snapshot);
        let _ = self.updates.send(MempoolUpdate::Reset {
            sequence: next_sequence,
        });
        self.status.store(StatusType::Syncing);
    }

    fn publish_closing(&self) {
        let current = self.current.load_full();
        let next_sequence = current.event_sequence.saturating_add(1);
        let _ = self.updates.send(MempoolUpdate::Closing {
            sequence: next_sequence,
        });
        self.status.store(StatusType::Closing);
    }
}
