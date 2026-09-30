//! Outputs delivered above the durable tip, staged and non-finalized alike
//!
//! - filled per delivered run (before its blocks are staged or applied), drained once a commit is
//!   on disk
//! - `imbl`: a resolve pins a copy in O(1) while delivery keeps inserting

use imbl::OrdMap;
use zaino_primitives::types::{Block, Height, OutPoint, Zatoshis};

use crate::key::OutputRow;

#[derive(Clone, Default)]
pub(crate) struct Pending {
    outputs: OrdMap<OutPoint, (Height, Zatoshis)>,
}

impl Pending {
    pub(crate) fn insert(&mut self, block: &Block) {
        let height = block.header().height;
        for tx in block.transactions() {
            for (vout, output) in (0..).zip(&tx.transparent.outputs) {
                let key = OutPoint { txid: tx.txid, vout };
                self.outputs.insert(key, (height, output.value));
            }
        }
    }

    pub(crate) fn value(&self, key: &OutPoint) -> Option<Zatoshis> {
        self.outputs.get(key).map(|(_, value)| *value)
    }

    /// Every output of a block at or below `tip` (last height, inclusive; `None` = none; what a
    /// commit makes durable), kept until it lands
    pub(crate) fn rows_through(&self, tip: Option<Height>) -> Vec<OutputRow> {
        self.outputs
            .iter()
            .filter(|(_, (height, _))| Some(*height) <= tip)
            .map(|(key, (_, value))| OutputRow { key: *key, value: *value })
            .collect()
    }

    /// Drops `written` (durable segments answer for them now)
    pub(crate) fn remove(&mut self, written: &[OutPoint]) {
        for key in written {
            self.outputs.remove(key);
        }
    }
}
