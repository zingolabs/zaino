//! Wire answers computed once per published view
//!
//! - every synced wallet asks the same tip questions right after each block (tree states, subtree
//!   roots): one computation per publication, then refcount clones
//! - keyed on the view's `Arc` identity: a new publication = a fresh memo, never an invalidation
//!   rule to get wrong (a view is immutable)
//! - one publication held: a request pinned on an older one replaces it (brief, at publication)

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex, OnceLock};

pub(crate) struct PerView<V, K, T> {
    last: Mutex<Option<Memo<V, K, T>>>,
}

struct Memo<V, K, T> {
    view: Arc<V>,
    answers: HashMap<K, Arc<OnceLock<T>>>,
}

impl<V, K, T> Default for PerView<V, K, T> {
    fn default() -> Self {
        Self { last: Mutex::new(None) }
    }
}

impl<V, K: Eq + Hash, T: Clone> PerView<V, K, T> {
    /// Already computed for `view`: the inline path (no hop, no permit)
    pub(crate) fn cached(&self, view: &Arc<V>, key: &K) -> Option<T> {
        let last = self.last.lock().expect("per-view memo poisoned");
        let memo = last.as_ref().filter(|memo| Arc::ptr_eq(&memo.view, view))?;
        memo.answers.get(key)?.get().cloned()
    }

    /// `compute` at most once per `(view, key)`
    ///
    /// - concurrent askers of one key block on the first (single flight: the herd after a block
    ///   costs one computation)
    /// - other keys proceed
    /// - blocking: call off the async workers when `compute` reads pages
    pub(crate) fn get_or_compute(&self, view: &Arc<V>, key: K, compute: impl FnOnce() -> T) -> T {
        let cell = {
            let mut last = self.last.lock().expect("per-view memo poisoned");
            let memo = match last.as_mut().filter(|memo| Arc::ptr_eq(&memo.view, view)) {
                Some(memo) => memo,
                None => last.insert(Memo { view: Arc::clone(view), answers: HashMap::new() }),
            };
            Arc::clone(memo.answers.entry(key).or_default())
        };
        cell.get_or_init(compute).clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_askers_of_one_key_share_one_computation() {
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

        let memo: PerView<u32, &str, usize> = PerView::default();
        let view = Arc::new(1u32);
        let computed = AtomicUsize::new(0);
        let compute = || {
            computed.fetch_add(1, SeqCst);
            // held open while the other askers arrive
            std::thread::sleep(std::time::Duration::from_millis(20));
            10
        };
        assert_eq!(memo.cached(&view, &"tip"), None, "nothing computed yet");

        let herd: Vec<usize> = std::thread::scope(|scope| {
            let askers: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| memo.get_or_compute(&view, "tip", compute)))
                .collect();
            askers.into_iter().map(|asker| asker.join().expect("asker")).collect()
        });

        assert_eq!(herd, [10; 8], "every asker answered");
        assert_eq!(computed.load(SeqCst), 1, "eight concurrent askers, one computation");
        assert_eq!(memo.cached(&view, &"tip"), Some(10), "then served inline");
        assert_eq!(memo.cached(&view, &"roots"), None, "another key = its own computation");
    }

    #[test]
    fn a_new_view_starts_empty_and_drops_the_older_one() {
        let memo: PerView<u32, &str, usize> = PerView::default();
        let (first, second) = (Arc::new(1u32), Arc::new(2u32));
        memo.get_or_compute(&first, "tip", || 10);

        let fresh = memo.cached(&second, &"tip");
        let computed = memo.get_or_compute(&second, "tip", || 20);

        assert_eq!(fresh, None, "a new publication knows nothing");
        assert_eq!(computed, 20, "computed for the new publication");
        assert_eq!(memo.cached(&first, &"tip"), None, "the older publication dropped");
    }
}
