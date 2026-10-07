//! One block of the final stream: what every index writer commits (`docs/design/nfs.md` §2)
//!
//! - every height once, ascending, never retracted
//! - `folds` = `None` below the NFS root (writer folds), `Some` above it (folded once by the NFS)

use std::sync::Arc;

use zaino_persistence::Changes;
use zaino_primitives::types::Block;

use crate::{PerIndex, Weight};

pub struct Final {
    pub block: Arc<Block>,
    pub folds: Option<Arc<Folds>>,
}

/// Each enabled index's `Changes` for one block
pub type Folds = PerIndex<Changes>;

impl Weight for Final {
    fn weight(&self) -> usize {
        let folds = self.folds.iter().flat_map(|folds| folds.iter());
        self.block.weight() + folds.map(|(_, changes)| changes.bytes()).sum::<usize>()
    }
}
