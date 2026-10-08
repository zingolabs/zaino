//! [`NfsProgress`]: blocks handed to the indexes, sampled at report time (decision 5)
//!
//! - Atomics, not a watch (bulk sync hands thousands of blocks a second; readers sample)

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use zaino_primitives::types::Height;

/// Cheap clone, one tally per NFS
#[derive(Debug, Clone, Default)]
pub struct NfsProgress(Arc<Tally>);

/// `handed` 0 = none, else height + 1
#[derive(Debug, Default)]
struct Tally {
    handed: AtomicU64,
    blocks: AtomicU64,
}

impl NfsProgress {
    /// Last block handed (folded, or sent final unfolded); rewinds on a reorg
    pub fn handed(&self) -> Option<Height> {
        Height::try_from(self.0.handed.load(Ordering::Relaxed).checked_sub(1)?).ok()
    }

    /// Blocks handed since boot (a reorg's branch counts again)
    pub fn blocks(&self) -> u64 {
        self.0.blocks.load(Ordering::Relaxed)
    }

    pub(crate) fn hand(&self, height: Height) {
        self.0.handed.store(u64::from(u32::from(height)) + 1, Ordering::Relaxed);
        self.0.blocks.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(any(test, feature = "testing"))]
impl NfsProgress {
    /// No NFS behind it: `handed` = `at`, one block counted per hand
    pub fn fixed(at: Option<Height>) -> Self {
        let progress = Self::default();
        if let Some(at) = at {
            progress.hand(at);
        }
        progress
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handed_is_the_last_height_and_blocks_count_every_hand() {
        let progress = NfsProgress::default();
        assert_eq!((progress.handed(), progress.blocks()), (None, 0));
        let at = |h: u32| Height::try_from(h).expect("h");
        for h in [0, 1, 2, 1] {
            progress.hand(at(h));
        }
        assert_eq!((progress.handed(), progress.blocks()), (Some(at(1)), 4), "rewound by a reorg");
        assert_eq!(progress.clone().handed(), Some(at(1)), "clones share one tally");
    }
}
