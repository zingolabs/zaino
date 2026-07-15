//! The mempool service: a single writer task that maintains the coherent,
//! bounded mempool read model and publishes it as immutable snapshots.
//!
//! # Coherence
//!
//! The service tracks two chain tips: the validator/mempool-source tip ("V",
//! from [`MempoolSource::get_mempool_source_tip`]) and the non-finalized-state
//! epoch ("NS", from [`NfsEpochObserver::current_epoch`]). It mutates the
//! transaction set **only while V and NS agree**, and re-checks agreement after
//! fetching so an update built against a stale tip is discarded. Any tip change,
//! disagreement, unavailability, or source error freezes the set: the last
//! coherent snapshot stays readable and no live deltas are emitted.

use std::collections::HashSet;
use std::sync::Arc;

use arc_swap::ArcSwap;
use futures::{stream, StreamExt as _};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use zaino_common::status::{NamedAtomicStatus, StatusType};

use crate::config::MempoolConfig;
use crate::entry::MempoolEntry;
use crate::event::MempoolEvent;
use crate::ports::{MempoolSource, MempoolTxMeta, NfsEpochObserver, NonFinalizedEpoch};
use crate::snapshot::{
    FreezeReason, MempoolCompleteness, MempoolMode, MempoolSnapshot, ObservedTips, TipChange,
    ValidatorTip,
};
use crate::subscriber::MempoolSubscriber;
use crate::MempoolError;

/// Internal outcome of an attempted transaction-set update.
#[derive(Debug)]
enum MempoolUpdateError {
    /// V and NS no longer agree on the target epoch; discard and freeze.
    CoherenceLost { observed_tips: ObservedTips },
    /// The source failed; freeze the existing set.
    SourceError,
    /// Applying the update would breach a configured capacity bound.
    CapacityLimited,
}

/// The mempool read-model service.
///
/// Generic over its two ports so the core has no `zaino-state` dependency.
pub struct MempoolService<S: MempoolSource, N: NfsEpochObserver> {
    source: S,
    nfs: N,
    current: Arc<ArcSwap<MempoolSnapshot>>,
    events: broadcast::Sender<Arc<MempoolEvent>>,
    config: MempoolConfig,
    status: NamedAtomicStatus,
    cancel: CancellationToken,
    task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl<S: MempoolSource, N: NfsEpochObserver> MempoolService<S, N> {
    /// Spawn the service and its background update task.
    pub fn spawn(source: S, nfs: N, config: MempoolConfig, cancel: CancellationToken) -> Arc<Self> {
        let (events, _) = broadcast::channel(config.event_buffer_len);

        let service = Arc::new(Self {
            source,
            nfs,
            current: Arc::new(ArcSwap::from_pointee(MempoolSnapshot::empty_not_ready())),
            events,
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

    /// A cheap, cloneable read handle onto the mempool.
    pub fn subscriber(&self) -> MempoolSubscriber {
        MempoolSubscriber::new(
            Arc::clone(&self.current),
            self.events.clone(),
            self.config.clone(),
            self.status.clone(),
        )
    }

    /// Current service status.
    pub fn status(&self) -> StatusType {
        self.status.load()
    }

    /// Signal shutdown and abort the background task.
    pub fn close(&self) {
        self.status.store(StatusType::Closing);
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
        let current = self.current.load_full();
        let previous_tips = current.observed_tips;

        let observed = match self.observe_tips().await {
            Ok(tips) => tips,
            Err(_) => {
                self.publish_frozen_snapshot(previous_tips, FreezeReason::SourceError);
                return;
            }
        };

        let tips_changed = Self::classify_tip_change(previous_tips, observed) != TipChange::None;

        if tips_changed {
            let reason = Self::freeze_reason_from_tips(previous_tips, observed);
            self.publish_frozen_snapshot(observed, reason);
        }

        let Some(target_epoch) = observed.agree() else {
            if !tips_changed {
                self.publish_frozen_snapshot(observed, FreezeReason::TipsDiverged);
            }
            return;
        };

        match self.update_for_epoch(target_epoch).await {
            Ok(()) => {}
            Err(MempoolUpdateError::CoherenceLost { observed_tips }) => {
                self.publish_frozen_snapshot(observed_tips, FreezeReason::TipsDiverged);
            }
            Err(MempoolUpdateError::SourceError) => {
                self.publish_frozen_snapshot(observed, FreezeReason::SourceError);
            }
            Err(MempoolUpdateError::CapacityLimited) => {
                self.publish_frozen_snapshot(observed, FreezeReason::CapacityLimited);
            }
        }
    }

    // ---- tip observation ------------------------------------------------

    fn current_ns_epoch(&self) -> Option<NonFinalizedEpoch> {
        self.nfs.current_epoch()
    }

    async fn current_validator_tip(&self) -> Result<Option<ValidatorTip>, MempoolError> {
        Ok(self
            .source
            .get_mempool_source_tip()
            .await?
            .map(|best_tip| ValidatorTip { best_tip }))
    }

    async fn observe_tips(&self) -> Result<ObservedTips, MempoolError> {
        // Observe NS first (cheap, local) then V (a source call), so the pair is
        // as close to simultaneous as a single tick allows.
        let non_finalized = self.current_ns_epoch();
        let validator = self.current_validator_tip().await?;
        Ok(ObservedTips {
            validator,
            non_finalized,
        })
    }

    fn classify_tip_change(previous: ObservedTips, next: ObservedTips) -> TipChange {
        let validator_changed = previous.validator != next.validator;
        let ns_changed = previous.non_finalized != next.non_finalized;
        match (validator_changed, ns_changed) {
            (false, false) => TipChange::None,
            (true, false) => TipChange::ValidatorChanged,
            (false, true) => TipChange::NonFinalizedChanged,
            (true, true) => TipChange::BothChanged,
        }
    }

    fn freeze_reason_from_tips(old_tips: ObservedTips, new_tips: ObservedTips) -> FreezeReason {
        if new_tips.non_finalized.is_none() {
            return FreezeReason::NonFinalizedUnavailable;
        }
        if new_tips.validator.is_none() {
            return FreezeReason::ValidatorTipUnavailable;
        }
        if new_tips.disagree() {
            return FreezeReason::TipsDiverged;
        }
        match Self::classify_tip_change(old_tips, new_tips) {
            TipChange::ValidatorChanged => FreezeReason::ValidatorTipChanged,
            TipChange::NonFinalizedChanged => FreezeReason::NonFinalizedTipChanged,
            TipChange::BothChanged => FreezeReason::BothTipsChanged,
            TipChange::None => FreezeReason::TipsDiverged,
        }
    }

    // ---- freeze / close publication ------------------------------------

    fn publish_frozen_snapshot(&self, observed_tips: ObservedTips, reason: FreezeReason) {
        let current = self.current.load_full();
        let next_sequence = current.event_sequence.saturating_add(1);

        // Reuse every Arc; the transaction set is not mutated when frozen.
        let completeness = match reason {
            FreezeReason::SourceError => MempoolCompleteness::IncompleteSourceError,
            FreezeReason::CapacityLimited => MempoolCompleteness::IncompleteCapacityLimited,
            _ => current.completeness,
        };

        let frozen = Arc::new(MempoolSnapshot {
            mode: MempoolMode::Frozen {
                valid_for: current.valid_for,
                reason,
            },
            valid_for: current.valid_for,
            observed_tips,
            mempool_generation: current.mempool_generation,
            event_sequence: next_sequence,
            by_txid: Arc::clone(&current.by_txid),
            txids_sorted: Arc::clone(&current.txids_sorted),
            entries_in_order: Arc::clone(&current.entries_in_order),
            tx_count: current.tx_count,
            raw_bytes: current.raw_bytes,
            cost_bytes: current.cost_bytes,
            completeness,
        });

        self.current.store(Arc::clone(&frozen));
        let _ = self.events.send(Arc::new(MempoolEvent::Frozen {
            sequence: next_sequence,
            snapshot: frozen,
            reason,
        }));
        self.status.store(StatusType::Syncing);
    }

    fn publish_closing(&self) {
        let current = self.current.load_full();
        let next_sequence = current.event_sequence.saturating_add(1);

        let closing = Arc::new(MempoolSnapshot {
            mode: MempoolMode::Closing,
            valid_for: current.valid_for,
            observed_tips: current.observed_tips,
            mempool_generation: current.mempool_generation,
            event_sequence: next_sequence,
            by_txid: Arc::clone(&current.by_txid),
            txids_sorted: Arc::clone(&current.txids_sorted),
            entries_in_order: Arc::clone(&current.entries_in_order),
            tx_count: current.tx_count,
            raw_bytes: current.raw_bytes,
            cost_bytes: current.cost_bytes,
            completeness: current.completeness,
        });

        self.current.store(closing);
        let _ = self.events.send(Arc::new(MempoolEvent::Closing {
            sequence: next_sequence,
        }));
        self.status.store(StatusType::Closing);
    }

    // ---- live update / thaw --------------------------------------------

    /// Reconcile (or incrementally update) the transaction set for `target_epoch`.
    ///
    /// The reconcile (`Frozen`/`NotReady` -> `Live`) and steady-state
    /// incremental-update paths share one body: both diff the source mempool
    /// against the current set under the two coherence guards. A `Frozen ->
    /// Live` transition falls out naturally when the current set isn't already
    /// live at this epoch.
    async fn update_for_epoch(
        &self,
        target_epoch: NonFinalizedEpoch,
    ) -> Result<(), MempoolUpdateError> {
        // Guard 1: V == target NS before any mempool data fetch.
        let before = self
            .observe_tips()
            .await
            .map_err(|_| MempoolUpdateError::SourceError)?;
        if before.agree() != Some(target_epoch) {
            return Err(MempoolUpdateError::CoherenceLost {
                observed_tips: before,
            });
        }

        let metadata = self
            .source
            .get_mempool_metadata()
            .await
            .map_err(|_| MempoolUpdateError::SourceError)?
            .ok_or(MempoolUpdateError::SourceError)?;

        let current = self.current.load_full();

        let source_txids: HashSet<_> = metadata.iter().map(|meta| meta.txid).collect();

        let removed: Vec<_> = current
            .by_txid
            .keys()
            .filter(|txid| !source_txids.contains(*txid))
            .copied()
            .collect();

        let added_meta: Vec<MempoolTxMeta> = metadata
            .into_iter()
            .filter(|meta| !current.by_txid.contains_key(&meta.txid))
            .collect();

        if added_meta.is_empty() && removed.is_empty() && current.is_live_for(target_epoch) {
            // Nothing changed and we're already live for this epoch: no publish.
            return Ok(());
        }

        let next_generation = current.mempool_generation.saturating_add(1);
        let added_entries = self
            .fetch_added_entries_bounded(added_meta, next_generation)
            .await?;

        // Guard 2: V and NS still agree on the same epoch after the fetch.
        let after = self
            .observe_tips()
            .await
            .map_err(|_| MempoolUpdateError::SourceError)?;
        if after.agree() != Some(target_epoch) {
            return Err(MempoolUpdateError::CoherenceLost {
                observed_tips: after,
            });
        }

        self.publish_live_snapshot(target_epoch, after, added_entries, removed)
    }

    async fn fetch_added_entries_bounded(
        &self,
        added: Vec<MempoolTxMeta>,
        next_generation: u64,
    ) -> Result<Vec<Arc<MempoolEntry>>, MempoolUpdateError> {
        let concurrency = self.config.max_concurrent_raw_fetches.max(1);
        let source = &self.source;

        let results = stream::iter(added)
            .map(|meta| async move {
                let raw = source
                    .get_raw_mempool_transaction(meta.txid)
                    .await
                    .map_err(|_| MempoolUpdateError::SourceError)?;

                // A `None` here is the normal race: the tx left the mempool
                // between listing and fetch. Skip it, don't fail the update.
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
            .collect::<Vec<Result<Option<Arc<MempoolEntry>>, MempoolUpdateError>>>()
            .await;

        let mut entries = Vec::new();
        for result in results {
            if let Some(entry) = result? {
                entries.push(entry);
            }
        }
        Ok(entries)
    }

    fn publish_live_snapshot(
        &self,
        target_epoch: NonFinalizedEpoch,
        observed_tips: ObservedTips,
        added_entries: Vec<Arc<MempoolEntry>>,
        removed: Vec<zebra_chain::transaction::Hash>,
    ) -> Result<(), MempoolUpdateError> {
        let current = self.current.load_full();

        let mut next_by_txid = current.by_txid.as_ref().clone();
        for txid in &removed {
            next_by_txid.remove(txid);
        }
        for entry in &added_entries {
            next_by_txid.insert(entry.txid, Arc::clone(entry));
        }

        let cost_bytes: u64 = next_by_txid.values().map(|entry| entry.cost()).sum();
        if cost_bytes > self.config.max_cost_bytes() {
            // Applying this update would breach the DoS backstop. Freeze at the
            // last coherent set and mark it incomplete rather than exceed the
            // bound or claim a complete-but-oversized view.
            return Err(MempoolUpdateError::CapacityLimited);
        }
        let raw_bytes: u64 = next_by_txid
            .values()
            .map(|entry| entry.raw_len as u64)
            .sum();

        let mut txids_sorted: Vec<_> = next_by_txid.keys().copied().collect();
        txids_sorted.sort_unstable_by(|a, b| a.0.cmp(&b.0));

        let entries_in_order: Vec<_> = txids_sorted
            .iter()
            .map(|txid| Arc::clone(&next_by_txid[txid]))
            .collect();

        let next_sequence = current.event_sequence.saturating_add(1);
        let next_generation = current.mempool_generation.saturating_add(1);
        let tx_count = next_by_txid.len();

        let snapshot = Arc::new(MempoolSnapshot {
            mode: MempoolMode::Live {
                valid_for: target_epoch,
            },
            valid_for: Some(target_epoch),
            observed_tips,
            mempool_generation: next_generation,
            event_sequence: next_sequence,
            by_txid: Arc::new(next_by_txid),
            txids_sorted: Arc::from(txids_sorted),
            entries_in_order: Arc::from(entries_in_order),
            tx_count,
            raw_bytes,
            cost_bytes,
            completeness: MempoolCompleteness::Complete,
        });

        self.current.store(Arc::clone(&snapshot));

        for txid in removed {
            let _ = self.events.send(Arc::new(MempoolEvent::Removed {
                sequence: next_sequence,
                valid_for: target_epoch,
                txid,
            }));
        }
        for entry in added_entries {
            let _ = self.events.send(Arc::new(MempoolEvent::Added {
                sequence: next_sequence,
                valid_for: target_epoch,
                entry,
            }));
        }
        let _ = self.events.send(Arc::new(MempoolEvent::Live {
            sequence: next_sequence,
            snapshot,
        }));

        self.status.store(StatusType::Ready);
        Ok(())
    }
}
