//! The coherence reconcile task: the long-lived loop that wakes on core
//! updates, NS-epoch changes and the fallback tick, and drives [`reconcile`].
//!
//! [`reconcile`]: super::CoherenceService::reconcile

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast;
use zaino_status::StatusType;

use zaino_mempool::ports::{Mempool, NfsEpochObserver};
use zaino_mempool::update::MempoolUpdate;

/// A freeze longer than this means the validator and Zaino tips have stopped agreeing.
const FREEZE_ESCALATION: Duration = Duration::from_secs(120);

impl<M: Mempool, N: NfsEpochObserver> super::CoherenceService<M, N> {
    /// Sets the frozen-seconds gauge, warning once per freeze that outlives
    /// [`FREEZE_ESCALATION`].
    fn report_frozen(&self, escalated: &mut bool) {
        let frozen_for = super::frozen_for(&self.frozen_since);
        metrics::gauge!(crate::metric_names::MEMPOOL_COHERENCE_FROZEN_SECONDS)
            .set(frozen_for.map_or(0.0, |frozen| frozen.as_secs_f64()));

        let escalating = frozen_for.is_some_and(|frozen| frozen >= FREEZE_ESCALATION);
        if escalating && !*escalated {
            tracing::warn!(
                frozen_for_secs = frozen_for.map_or(0, |frozen| frozen.as_secs()),
                "mempool coherence has been frozen far longer than a tip \
                 transition should take; tip-coherent reads are unavailable"
            );
        }
        *escalated = escalating;
    }

    /// The coherence reconcile task.
    ///
    /// One long-lived span, as with the core's poll loop: reconciles are
    /// sub-second and mostly no-ops, so the signal is in the freeze/thaw edges
    /// rather than in per-reconcile spans.
    #[tracing::instrument(name = "mempool_coherence_loop", skip_all)]
    pub(super) async fn run(self: Arc<Self>) {
        self.status.store(StatusType::Syncing);

        let mut updates = self.mempool.subscribe_updates();
        // The NS tip advances on Zaino's own sync, which does not always coincide
        // with a core update. Prefer the observer's wake signal — waiting for the
        // tick instead would freeze tip-coherent reads for that long after every
        // block — and keep the tick as a fallback for observers with no signal.
        let mut epoch_wake = self
            .nfs
            .as_ref()
            .and_then(|nfs| nfs.subscribe_epoch_changes());
        let mut interval = tokio::time::interval(self.config.poll_interval());
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        tracing::debug!(
            dual_tip = self.nfs.is_some(),
            epoch_wake = epoch_wake.is_some(),
            "mempool coherence loop started"
        );

        self.reconcile();
        let mut escalated = false;

        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    tracing::debug!("mempool coherence loop cancelled; publishing Closing");
                    self.publish_closing();
                    return;
                }
                _ = interval.tick() => {
                    self.reconcile();
                    self.report_frozen(&mut escalated);
                }
                _ = async {
                    match epoch_wake.as_mut() {
                        Some(rx) => {
                            let _ = rx.changed().await;
                        }
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    self.reconcile();
                }
                update = updates.recv() => {
                    match update {
                        Ok(MempoolUpdate::Closing { .. }) => {
                            self.publish_closing();
                            return;
                        }
                        // Reconcile on the batch boundary only. The core emits
                        // one message per added/removed txid and closes every
                        // batch with a `Reset`, so waking on each would mean
                        // thousands of reconciles for a single cleared block —
                        // and `reconcile` re-reads the core's snapshot wholesale
                        // anyway, so the per-txid messages carry nothing extra.
                        Ok(MempoolUpdate::Reset { .. })
                        | Err(broadcast::error::RecvError::Lagged(_)) => {
                            self.reconcile();
                        }
                        Ok(MempoolUpdate::Added { .. })
                        | Ok(MempoolUpdate::Removed { .. })
                        | Ok(MempoolUpdate::Lagged { .. }) => {}
                        Err(broadcast::error::RecvError::Closed) => return,
                    }
                }
            }
        }
    }
}
