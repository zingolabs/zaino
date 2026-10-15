//! Rendered finalised treestates, kept across snapshots.
//!
//! Rendering a stored frontier computes each pool's root, which costs
//! milliseconds where reading the frontier costs microseconds. A finalised
//! height's treestate never changes, so a rendered one is kept by height and
//! served again: the composer reads the treestate at the watermark as the seed
//! of every read above it, and wallets scanning the same recent range ask for
//! the same heights.
//!
//! Bounded, oldest insertion evicted first.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

use zaino_primitives::types::{Height, Treestate};

/// How many rendered treestates are kept. A rendered treestate is a few
/// kilobytes, so this bounds the cache to a few megabytes.
const CAPACITY: usize = 2048;

/// Rendered treestates by height, shared by every snapshot of one store reader.
#[derive(Default)]
pub(crate) struct RenderedTreestates {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    by_height: HashMap<Height, Arc<Treestate>>,
    insertion_order: VecDeque<Height>,
}

impl RenderedTreestates {
    /// The rendered treestate at `height`, if kept.
    pub(crate) fn get(&self, height: Height) -> Option<Arc<Treestate>> {
        self.inner().by_height.get(&height).cloned()
    }

    /// Keep `treestate` as the rendering at `height`, evicting the oldest entry
    /// when full.
    pub(crate) fn insert(&self, height: Height, treestate: Arc<Treestate>) {
        let mut inner = self.inner();
        if inner.by_height.insert(height, treestate).is_none() {
            inner.insertion_order.push_back(height);
        }
        while inner.insertion_order.len() > CAPACITY {
            if let Some(oldest) = inner.insertion_order.pop_front() {
                inner.by_height.remove(&oldest);
            }
        }
    }

    fn inner(&self) -> MutexGuard<'_, Inner> {
        // Every critical section is a few map operations that cannot panic
        // part-way, so a poisoned lock still guards a consistent cache.
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{RenderedTreestates, CAPACITY};
    use zaino_primitives::types::{BlockHash, Height, Treestate};

    fn at(height: u32) -> (Height, Arc<Treestate>) {
        let height = Height::try_from(height).expect("a valid test height");
        let treestate = Treestate {
            block_hash: BlockHash::from([0; 32]),
            height,
            time: 0,
            sapling: None,
            orchard: None,
            ironwood: None,
        };
        (height, Arc::new(treestate))
    }

    /// The cache holds at most `CAPACITY` heights, dropping the oldest first.
    #[test]
    fn the_oldest_insertion_is_evicted_first() {
        let cache = RenderedTreestates::default();
        let limit = u32::try_from(CAPACITY).expect("capacity fits u32");
        for height in 0..=limit {
            let (height, treestate) = at(height);
            cache.insert(height, treestate);
        }
        assert!(cache.get(at(0).0).is_none(), "the first height was evicted");
        assert!(cache.get(at(1).0).is_some());
        assert!(cache.get(at(limit).0).is_some());
    }

    /// Re-inserting a height does not count twice toward the bound.
    #[test]
    fn a_reinsert_does_not_grow_the_order() {
        let cache = RenderedTreestates::default();
        let (height, treestate) = at(5);
        cache.insert(height, Arc::clone(&treestate));
        cache.insert(height, treestate);
        assert_eq!(cache.inner().insertion_order.len(), 1);
    }
}
