//! Serving's handle on one index: the view its follower last published + its `synced` gate

use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio::sync::watch;

/// One index as its services read it (clone = share)
///
/// - one [`pin`](Self::pin) per request: every tier it reads comes from one publication
pub struct Served<V> {
    view: Arc<ArcSwap<V>>,
    synced: watch::Receiver<bool>,
}

impl<V> Clone for Served<V> {
    fn clone(&self) -> Self {
        Self { view: Arc::clone(&self.view), synced: self.synced.clone() }
    }
}

impl<V> std::fmt::Debug for Served<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Served").field("synced", &*self.synced.borrow()).finish_non_exhaustive()
    }
}

impl<V> Served<V> {
    /// `view` = what the follower republishes, `synced` = its gate
    pub fn new(view: Arc<ArcSwap<V>>, synced: watch::Receiver<bool>) -> Self {
        Self { view, synced }
    }

    /// `view` for good, synced (an index with no follower behind it: tests)
    pub fn fixed(view: V) -> Self {
        // sender dropped: `borrow` keeps the last value
        Self::new(Arc::new(ArcSwap::from_pointee(view)), watch::channel(true).1)
    }

    /// Latest view; `None` while syncing (a syncing index = one answer, never a partial one)
    pub fn pin(&self) -> Option<Arc<V>> {
        (*self.synced.borrow()).then(|| self.view.load_full())
    }

    /// Latest view, synced or not (final data, or the extent, answered while syncing)
    pub fn pin_any(&self) -> Arc<V> {
        self.view.load_full()
    }
}
