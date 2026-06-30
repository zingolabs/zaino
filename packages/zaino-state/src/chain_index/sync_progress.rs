//! Shared sync-progress snapshot for tip-proximity readiness checks.
//!
//! The chain-index sync loop flips its [`StatusType`] to `Syncing` at the top
//! of *every* iteration (even a steady-state re-check at the tip), so a
//! readiness probe driven off the raw status would flap `ready -> not ready ->
//! ready` on each cycle and churn the pod in and out of its k8s Service.
//!
//! [`SyncProgress`] is the stable signal instead, mirroring Zebra's health
//! component: the sync loop records the network tip it observed and the height
//! it has synced to, and a probe reports ready while the gap is small and the
//! tip is fresh. Being one or two blocks behind for a moment does not flip
//! readiness; falling far behind (initial sync, large re-org catch-up) or going
//! stale (a silently dead source that stops advancing the tip) does.
//!
//! [`StatusType`]: crate::status::StatusType

use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

/// A clone-safe, lock-free snapshot of the sync loop's progress toward the
/// network tip. All clones share one underlying state.
#[derive(Clone, Debug)]
pub struct SyncProgress {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    /// Most recently observed network/source tip height. Initialised to
    /// `u64::MAX` so [`SyncProgress::blocks_behind`] reports "fully behind"
    /// until the sync loop observes a real tip — a probe must never read
    /// "caught up" before the first observation.
    network_tip: AtomicU64,
    /// Highest height the sync loop has fully synced to.
    local_tip: AtomicU64,
    /// Milliseconds (relative to `base`) at which `local_tip` was last
    /// confirmed synced to the network tip. Drives the tip-age check.
    last_synced_millis: AtomicU64,
    /// Monotonic reference instant for `last_synced_millis`.
    base: Instant,
}

impl SyncProgress {
    /// Creates a tracker in the "not yet synced" state.
    pub fn new() -> Self {
        SyncProgress {
            inner: Arc::new(Inner {
                network_tip: AtomicU64::new(u64::MAX),
                local_tip: AtomicU64::new(0),
                last_synced_millis: AtomicU64::new(0),
                base: Instant::now(),
            }),
        }
    }

    /// Records the network tip height observed at the start of a sync
    /// iteration.
    pub fn record_network_tip(&self, height: u32) {
        self.inner
            .network_tip
            .store(height as u64, Ordering::Relaxed);
    }

    /// Records that the index has fully synced up to `height`, refreshing both
    /// the local tip and the tip-age clock.
    pub fn record_synced(&self, height: u32) {
        self.inner.local_tip.store(height as u64, Ordering::Relaxed);
        self.inner.last_synced_millis.store(
            self.inner.base.elapsed().as_millis() as u64,
            Ordering::Relaxed,
        );
    }

    /// Estimated blocks behind the network tip. Saturates at 0 (a local tip at
    /// or ahead of the last observed network tip is "caught up").
    pub fn blocks_behind(&self) -> u64 {
        let network = self.inner.network_tip.load(Ordering::Relaxed);
        let local = self.inner.local_tip.load(Ordering::Relaxed);
        network.saturating_sub(local)
    }

    /// Time since the local tip was last confirmed synced to the network tip.
    /// Grows without bound if the sync loop stops succeeding (e.g. a dead
    /// source), which a stale-tip readiness check can act on.
    pub fn tip_age(&self) -> Duration {
        self.inner
            .base
            .elapsed()
            .saturating_sub(Duration::from_millis(
                self.inner.last_synced_millis.load(Ordering::Relaxed),
            ))
    }
}

impl Default for SyncProgress {
    fn default() -> Self {
        Self::new()
    }
}

/// Exposes a backend subscriber's [`SyncProgress`] handle.
///
/// Implemented by the service subscribers (both wrap a
/// [`NodeBackedChainIndexSubscriber`]) so a generic caller — e.g. the daemon's
/// health endpoint — can obtain the tip-proximity signal without naming the
/// concrete backend.
///
/// [`NodeBackedChainIndexSubscriber`]: crate::chain_index::NodeBackedChainIndexSubscriber
pub trait ChainSyncProgress {
    /// Returns a clone of the shared sync-progress handle.
    fn sync_progress(&self) -> SyncProgress;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_tracker_is_fully_behind() {
        let p = SyncProgress::new();
        // No network tip observed yet -> never reads as caught up.
        assert_eq!(p.blocks_behind(), u64::MAX);
    }

    #[test]
    fn blocks_behind_tracks_gap() {
        let p = SyncProgress::new();
        p.record_network_tip(1_000);
        p.record_synced(900);
        assert_eq!(p.blocks_behind(), 100);
        p.record_synced(1_000);
        assert_eq!(p.blocks_behind(), 0);
    }

    #[test]
    fn local_tip_ahead_saturates_to_zero() {
        let p = SyncProgress::new();
        p.record_network_tip(500);
        p.record_synced(500);
        // A later observation lagging the synced height must not underflow.
        p.record_network_tip(499);
        assert_eq!(p.blocks_behind(), 0);
    }

    #[test]
    fn record_synced_resets_tip_age() {
        let p = SyncProgress::new();
        p.record_synced(10);
        // Freshly synced: age is ~0, comfortably under any sane threshold.
        assert!(p.tip_age() < Duration::from_secs(1));
    }
}
