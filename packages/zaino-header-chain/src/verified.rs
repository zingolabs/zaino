//! [`VerifiedChain`]: the one value everything downstream reads (`verified-chain.md` §3)
//!
//! - clone = refcounts (`imbl` path, store view at the final tip): a holder's answers never change

use std::cmp::Reverse;

use zaino_primitives::types::{BlockHash, BlockRef, Height};

use crate::chain::{BestTip, Node};
use crate::store::{HeaderView, Record};

/// - `above[i]` = best branch at `final + 1 + i`; at or below the final tip: the store's view
/// - `nodes`, `leaves` = the header tree above the final tip, every branch (side ones: [`Fork`])
#[derive(Debug, Clone)]
pub struct VerifiedChain {
    best: BestTip,
    final_tip: Option<BlockRef>,
    above: imbl::Vector<Record>,
    finals: HeaderView,
    nodes: imbl::HashMap<BlockHash, Node>,
    leaves: imbl::HashSet<BlockHash>,
}

/// Side branch held beside the best: `from` = its best-chain parent (at or above the final tip)
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
        final_tip: Option<BlockRef>,
        above: imbl::Vector<Record>,
        finals: HeaderView,
        nodes: imbl::HashMap<BlockHash, Node>,
        leaves: imbl::HashSet<BlockHash>,
    ) -> Self {
        assert_eq!(finals.tip(), final_tip, "H5: the store view sits at the final tip");
        Self { best, final_tip, above, finals, nodes, leaves }
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
            let height = lowest.height.checked_sub(1).expect("genesis is on every best chain");
            let from = BlockRef { hash: lowest.parent, height };
            Fork { from, tip: leaf.at(), cumulative_work: leaf.record.cumulative_work }
        };
        leaves.into_iter().map(fork).collect()
    }

    /// Side blocks from the fork's `from` (exclusive) up to `tip`; empty = `tip` not a side block
    pub fn branch(&self, tip: &BlockHash) -> Vec<BlockRef> {
        self.off_best(*tip).iter().rev().map(|node| node.at()).collect()
    }

    /// On the best chain (final included), or a side block held above the final tip
    pub fn holds(&self, at: BlockRef) -> bool {
        let side = self.nodes.get(&at.hash).is_some_and(|node| node.height == at.height);
        side || self.hash_at(at.height) == Some(at.hash)
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

    /// Never moves back (`None` = nothing final yet)
    pub fn final_tip(&self) -> Option<BlockRef> {
        self.final_tip
    }

    /// Best-chain hash at `height` (`None` = above the best tip)
    pub fn hash_at(&self, height: Height) -> Option<BlockHash> {
        self.header_at(height).map(|record| record.hash)
    }

    /// Best-chain header at `height`: hash, merkle root, time, nBits, cumulative work
    pub fn header_at(&self, height: Height) -> Option<Record> {
        if height > self.best.block.height {
            return None;
        }
        let base = self.final_tip.map_or(Height::GENESIS, |tip| tip.height.next());
        match u32::from(height).checked_sub(u32::from(base)) {
            Some(above) => self.above.get(above as usize).copied(),
            None => self.finals.record(height),
        }
    }

    /// zcashd's `getheaders` locator: the best tip, ten consecutive ancestors, then doubling
    /// steps, ending at the final tip (genesis while nothing is final)
    pub fn locator(&self) -> Vec<BlockHash> {
        let floor = self.final_tip.map_or(0, |tip| u32::from(tip.height));
        let mut at = u32::from(self.best.block.height);
        let mut step = 1;
        let mut locator = Vec::new();
        loop {
            let height = Height::try_from(at).expect("between the floor and the best tip");
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

    /// `path` (a `testing::Chain`'s, genesis first) verified under regtest rules, nothing final
    #[cfg(feature = "testing")]
    pub fn regtest(path: &[zaino_primitives::types::Block]) -> Self {
        let genesis = path.first().expect("a path holds genesis").header().hash;
        let depth = zaino_primitives::types::ReorgDepth::CONSENSUS;
        let mut chain = crate::HeaderChain::regtest_in_memory(genesis, depth);
        chain.insert_blocks(path).expect("a testing::Chain path verifies");
        chain.verified().expect("genesis verified")
    }
}
