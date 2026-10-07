//! One block of the final stream: what every index writer commits (`docs/design/nfs.md` §2)
//!
//! - every height once, ascending, never retracted
//! - `folds` = `None` below the NFS root (the writer folds), `Some` above it (folded once by the NFS)

use std::sync::Arc;

use zaino_persistence::{Changes, IndexKind};
use zaino_primitives::types::Block;

use crate::Weight;

pub struct Final {
    pub block: Arc<Block>,
    pub folds: Option<Arc<Folds>>,
}

/// Each enabled index's `Changes` for one block
#[derive(Debug, Default)]
pub struct Folds(Vec<(IndexKind, Changes)>);

impl Folds {
    /// Panics on a second `Changes` for `index` (one fold per index per block)
    pub fn insert(&mut self, index: IndexKind, changes: Changes) {
        assert!(self.get(index).is_none(), "{}: folded twice", index.name());
        self.0.push((index, changes));
    }

    pub fn get(&self, index: IndexKind) -> Option<&Changes> {
        self.0.iter().find(|(kind, _)| *kind == index).map(|(_, changes)| changes)
    }
}

impl Weight for Final {
    fn weight(&self) -> usize {
        let folds = self.folds.iter().flat_map(|folds| &folds.0);
        self.block.weight() + folds.map(|(_, changes)| changes.bytes()).sum::<usize>()
    }
}
