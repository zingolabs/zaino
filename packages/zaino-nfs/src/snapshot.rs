//! The pinned window view — a coherent snapshot of the reorg window.
//!
//! Wraps a [`Chain`] (itself an O(1)-cloneable persistent vector), so pinning is
//! a cheap clone: the view stays fixed as the live window advances underneath
//! it. Implements the `NfsSpine` reads over the vendored chain; the facet traits
//! (spend/address re-derivation) and the tree-size projection land next.
//!
// Not yet constructed — the follow loop (`window`) produces this from the live
// chain in a following commit. Remove once wired.
#![allow(dead_code)]

use zaino_core::{BlockHash, BlockId, CompactBlock, ForkPoint, Height, Locator};

use crate::chain::Chain;
use crate::NfsSpine;

/// A pinned, reorg-coherent view of the recent window at one instant.
///
/// Constructed from a non-empty [`Chain`]: a `Ready` window always has at least
/// one block (an empty recent window is `Syncing`, not `Ready`).
#[derive(Clone)]
pub(crate) struct WindowSnapshot {
    chain: Chain,
}

impl WindowSnapshot {
    pub(crate) fn new(chain: Chain) -> Self {
        Self { chain }
    }
}

impl NfsSpine for WindowSnapshot {
    fn tip(&self) -> BlockId {
        let b = self.chain.last().expect("a Ready window is non-empty");
        BlockId {
            height: to_height(b.height),
            hash: b.hash,
        }
    }

    fn range(&self) -> (Height, Height) {
        let tip = self
            .chain
            .tip_height()
            .expect("a Ready window is non-empty");
        (to_height(self.chain.start), to_height(tip))
    }

    fn compact_block(&self, _height: Height) -> Option<CompactBlock> {
        // Seam: the serving `CompactBlock` = `PreIndexCompactBlock` + the
        // `ChainMetadata` sapling/orchard tree sizes, which are a running fold
        // seeded from the FS boundary tree state. Lands with tree-size seeding.
        todo!("project PreIndexCompactBlock -> CompactBlock via seeded tree sizes")
    }

    fn height_of(&self, hash: BlockHash) -> Option<Height> {
        // Bounded O(n) scan (~101 blocks): the window has no hash index.
        self.chain
            .iter()
            .find(|(_, b)| b.hash == hash)
            .map(|(h, _)| to_height(h))
    }

    fn fork_point(&self, _locator: Locator) -> Option<ForkPoint> {
        // Net-new (Q2): no counterpart in the vendored store.
        todo!("fork-point over the window vs a client locator")
    }

    fn chain_tips(&self) -> Vec<BlockId> {
        // One best tip until the side-branch set (Q2) lands.
        vec![self.tip()]
    }
}

/// Window heights are real block heights, always valid.
fn to_height(h: u32) -> Height {
    Height::try_from(h).expect("chain heights are valid")
}

#[cfg(test)]
mod tests {
    use zaino_core::PreIndexCompactBlock;

    use super::*;

    fn block(height: u32, prev: BlockHash) -> PreIndexCompactBlock {
        PreIndexCompactBlock {
            hash: BlockHash::from([height as u8; 32]),
            prev_hash: prev,
            height,
            time: 0,
            bits: 0,
            transactions: Vec::new(),
        }
    }

    fn window(start: u32, n: u32) -> WindowSnapshot {
        let mut chain = Chain::new(start);
        let mut prev = BlockHash::from([0u8; 32]);
        for h in start..start + n {
            let b = block(h, prev);
            prev = b.hash;
            chain = chain.push_back(b);
        }
        WindowSnapshot::new(chain)
    }

    #[test]
    fn spine_reports_tip_and_range_over_the_chain() {
        // window covers heights 101..=105
        let w = window(101, 5);
        assert_eq!(w.tip().height, to_height(105));
        assert_eq!(w.range(), (to_height(101), to_height(105)));
    }

    #[test]
    fn height_of_resolves_a_windowed_hash_and_misses_outside() {
        let w = window(101, 5);
        assert_eq!(w.height_of(BlockHash::from([103u8; 32])), Some(to_height(103)));
        assert_eq!(w.height_of(BlockHash::from([200u8; 32])), None);
    }

    #[test]
    fn chain_tips_is_the_single_best_tip() {
        let w = window(101, 3);
        assert_eq!(w.chain_tips(), vec![w.tip()]);
    }
}
