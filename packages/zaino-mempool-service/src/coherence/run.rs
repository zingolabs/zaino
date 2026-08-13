//! The coherence background task: wake on core updates, NS-epoch changes or the
//! fallback tick, and reconcile.

use std::sync::Arc;

use tokio::sync::broadcast;
use zaino_status::StatusType;

use zaino_mempool::ports::{Mempool, NfsEpochObserver};
use zaino_mempool::update::MempoolUpdate;

impl<M: Mempool, N: NfsEpochObserver> super::CoherenceService<M, N> {
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
        let mut interval = tokio::time::interval(self.config.poll_interval.get());
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        self.reconcile();

        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    self.publish_closing();
                    return;
                }
                _ = interval.tick() => {
                    self.reconcile();
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
