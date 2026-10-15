//! [`VerifiedChain`]: the one value everything downstream reads
//!
//! - clone = refcounts (`imbl`): a holder's answers never change
//! - at or below the finalized tip = finalized by definition (a trusted validator held it); only
//!   the newest `2 · depth` finalized headers answer by height

use std::cmp::Reverse;

use zaino_primitives::types::BlockRef;
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::BlockHash;

use crate::chain::{BestTip, Node, Record};

/// - `above[i]` = best branch at `finalized + 1 + i`; `finalized_headers` = the newest finalized
///   headers, oldest first, the finalized tip last
/// - `nodes`, `leaves` = the header tree above the finalized tip, every branch (side ones:
///   [`Fork`])
#[derive(Debug, Clone)]
pub struct VerifiedChain {
    best: BestTip,
    above: imbl::Vector<Record>,
    finalized_headers: imbl::Vector<(BlockHeight, Record)>,
    nodes: imbl::HashMap<BlockHash, Node>,
    leaves: imbl::HashSet<BlockHash>,
}

/// Side branch held beside the best: `from` = its best-chain parent (at or above the finalized tip)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fork {
    pub from: BlockRef,
    pub tip: BlockRef,
    pub cumulative_work: u128,
}

/// zcashd `CChain::GetLocator`: the step doubles once the locator holds more than this many
const CONSECUTIVE: usize = 10;

impl VerifiedChain {
    pub(crate) fn new(
        best: BestTip,
        above: imbl::Vector<Record>,
        finalized_headers: imbl::Vector<(BlockHeight, Record)>,
        nodes: imbl::HashMap<BlockHash, Node>,
        leaves: imbl::HashSet<BlockHash>,
    ) -> Self {
        assert!(
            !finalized_headers.is_empty(),
            "H5: a published chain has a finalized tip (its anchor)"
        );
        Self { best, above, finalized_headers, nodes, leaves }
    }

    /// One per side leaf (≤ `SIDE_TIPS`, H4), most work first, first received on a tie
    pub fn forks(&self) -> Vec<Fork> {
        let best = self.best.block.hash;
        let mut leaves: Vec<&Node> = self
            .leaves
            .iter()
            .filter(|leaf| **leaf != best)
            .map(|leaf| &self.nodes[leaf])
            .collect();
        leaves.sort_by_key(|leaf| (Reverse(leaf.record.cumulative_work), leaf.received));
        let fork = |leaf: &Node| {
            let lowest = *self.off_best(leaf.record.hash).last().expect("a side leaf is off best");
            let below = u32::from(lowest.height).checked_sub(1);
            let height =
                BlockHeight::from(below.expect("a side node sits above the finalized tip"));
            let from = BlockRef { hash: lowest.parent, height };
            Fork { from, tip: leaf.block_ref(), cumulative_work: leaf.record.cumulative_work }
        };
        leaves.into_iter().map(fork).collect()
    }

    /// Side blocks from the fork's `from` (exclusive) up to `tip`; empty = `tip` not a side block
    pub fn branch(&self, tip: &BlockHash) -> Vec<BlockRef> {
        self.off_best(*tip).iter().rev().map(|node| node.block_ref()).collect()
    }

    /// On the best chain (finalized included), or a side block held above the finalized tip
    pub fn holds(&self, at: BlockRef) -> bool {
        let side = self.nodes.get(&at.hash).is_some_and(|node| node.height == at.height);
        side || self.on_best(at)
    }

    /// On the best chain: a held header's hash, or at or below the finalized tip past the headers
    /// kept (finalized by definition)
    pub fn on_best(&self, at: BlockRef) -> bool {
        match self.header_at(at.height) {
            Some(record) => record.hash == at.hash,
            None => at.height <= self.finalized_tip().height,
        }
    }

    /// Side nodes from `tip` down to the best chain, `tip` first
    fn off_best(&self, tip: BlockHash) -> Vec<&Node> {
        let mut walked = Vec::new();
        let mut at = tip;
        while let Some(node) = self.nodes.get(&at) {
            if self.hash_at(node.height) == Some(at) {
                break;
            }
            walked.push(node);
            at = node.parent;
        }
        walked
    }

    pub fn best(&self) -> BlockRef {
        self.best.block
    }

    /// Never moves back but on a re-anchor (a trusted validator far ahead)
    pub fn finalized_tip(&self) -> BlockRef {
        let (height, record) =
            self.finalized_headers.back().expect("a published chain has a finalized tip");
        BlockRef { hash: record.hash, height: *height }
    }

    /// Best-chain hash at `height` (`None` = above the best tip, or finalized below the headers
    /// kept)
    pub fn hash_at(&self, height: BlockHeight) -> Option<BlockHash> {
        self.header_at(height).map(|record| record.hash)
    }

    /// Best-chain header at `height`: hash, merkle root, time
    pub fn header_at(&self, height: BlockHeight) -> Option<Record> {
        if height > self.best.block.height {
            return None;
        }
        let (oldest, _) =
            self.finalized_headers.front().expect("a published chain has a finalized tip");
        let at = u32::from(height).checked_sub(u32::from(*oldest))? as usize;
        match at.checked_sub(self.finalized_headers.len()) {
            Some(above) => self.above.get(above).copied(),
            None => self.finalized_headers.get(at).map(|(_, record)| *record),
        }
    }

    /// zcashd's `getheaders` locator: the best tip, ten consecutive ancestors, then doubling
    /// steps, ending at the finalized tip
    pub fn locator(&self) -> Vec<BlockHash> {
        let floor = u32::from(self.finalized_tip().height);
        let mut at = u32::from(self.best.block.height);
        let mut step = 1;
        let mut locator = Vec::new();
        loop {
            let height = BlockHeight::from(at);
            locator.push(self.hash_at(height).expect("every height up to the best is held"));
            if at == floor {
                return locator;
            }
            at = at.saturating_sub(step).max(floor);
            if locator.len() > CONSECUTIVE {
                step = step.saturating_mul(2);
            }
        }
    }
}
