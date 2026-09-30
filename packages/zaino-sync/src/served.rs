//! Serving's handle on one index: the view its loop last published + its `synced` gate

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use arc_swap::ArcSwap;
use tokio::sync::watch;

/// Views an index's services pinned since boot (one per request answered), shared by every clone
#[derive(Debug, Clone, Default)]
pub struct Reads(Arc<AtomicU64>);

impl Reads {
    pub fn total(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }

    fn count(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

/// One index as its services read it (clone = share)
///
/// - one [`pin`](Self::pin) or [`pin_any`](Self::pin_any) per request: every tier it reads comes
///   from one publication, and it counts once in [`Reads`]
pub struct Served<V> {
    view: Arc<ArcSwap<V>>,
    synced: watch::Receiver<bool>,
    reads: Reads,
}

impl<V> Clone for Served<V> {
    fn clone(&self) -> Self {
        Self {
            view: Arc::clone(&self.view),
            synced: self.synced.clone(),
            reads: self.reads.clone(),
        }
    }
}

impl<V> std::fmt::Debug for Served<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Served").field("synced", &self.synced()).finish_non_exhaustive()
    }
}

impl<V> Served<V> {
    /// `view` = what the index republishes, `synced` = its gate; counts into its own [`Reads`]
    pub fn new(view: Arc<ArcSwap<V>>, synced: watch::Receiver<bool>) -> Self {
        Self::counted(view, synced, Reads::default())
    }

    /// [`new`](Self::new), counting into `reads` (the index's, shared by every handle)
    pub(crate) fn counted(
        view: Arc<ArcSwap<V>>,
        synced: watch::Receiver<bool>,
        reads: Reads,
    ) -> Self {
        Self { view, synced, reads }
    }

    /// `view` for good, synced (no index loop behind it)
    pub fn fixed(view: V) -> Self {
        // sender dropped: `borrow` keeps the last value
        Self::new(Arc::new(ArcSwap::from_pointee(view)), watch::channel(true).1)
    }

    /// Latest view; `None` while syncing (a syncing index = one answer, never a partial one)
    pub fn pin(&self) -> Option<Arc<V>> {
        self.synced().then(|| self.pin_any())
    }

    /// Latest view, synced or not (final data, or the extent, answered while syncing)
    pub fn pin_any(&self) -> Arc<V> {
        self.reads.count();
        self.view.load_full()
    }

    /// The serving gate, without pinning a view
    pub fn synced(&self) -> bool {
        *self.synced.borrow()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every pinned view counts once, across clones; a refused pin counts nothing
    #[test]
    fn reads_count_answered_pins_shared_by_every_handle() {
        let (gate, synced) = watch::channel(false);
        let reads = Reads::default();
        let served = Served::counted(Arc::new(ArcSwap::from_pointee(7)), synced, reads.clone());
        let clone = served.clone();

        assert!(served.pin().is_none(), "syncing refuses");
        assert_eq!(reads.total(), 0, "a refusal is not a read");
        assert_eq!(*served.pin_any(), 7);
        gate.send_replace(true);
        assert_eq!(clone.pin().as_deref(), Some(&7));
        assert!(clone.synced() && served.synced());
        assert_eq!(reads.total(), 2, "pin_any and pin, across both handles");
    }
}
