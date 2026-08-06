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

use crate::chain::{Chain, HasHeader};
use crate::NfsSpine;

// The window stores the **domain block**: `CompactBlock`, which *is* the
// composition `PreIndexCompactBlock ⊕ ChainMetadata` — the ingestion format
// (from Zebra) enriched with the derived commitment-tree sizes. It carries
// everything the window's operations need: the header (reorg), the transparent
// I/O in its `PreIndexCompactTx`s (facets), and the tree sizes (serving). The
// `PreIndexCompactBlock → CompactBlock` composition happens once, at ingestion
// in the follow loop; the window just stores and serves the result.
impl HasHeader for CompactBlock {
    fn hash(&self) -> BlockHash {
        self.hash
    }
    fn prev_hash(&self) -> BlockHash {
        self.prev_hash
    }
    fn height(&self) -> u32 {
        self.height
    }
}

/// A pinned, reorg-coherent view of the recent window at one instant.
///
/// Constructed from a non-empty chain: a `Ready` window always has at least one
/// block (an empty recent window is `Syncing`, not `Ready`).
#[derive(Clone)]
pub(crate) struct WindowSnapshot {
    chain: Chain<CompactBlock>,
}

impl WindowSnapshot {
    pub(crate) fn new(chain: Chain<CompactBlock>) -> Self {
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

    fn compact_block(&self, height: Height) -> Option<CompactBlock> {
        // The stored element *is* the serving block (tree sizes composed at
        // ingestion) — just clone it out of the pinned chain.
        self.chain.get(u32::from(height)).cloned()
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
    use zaino_core::ChainMetadata;

    use super::*;

    fn block(height: u32, prev: BlockHash) -> CompactBlock {
        CompactBlock {
            hash: BlockHash::from([height as u8; 32]),
            prev_hash: prev,
            height,
            time: 0,
            bits: 0,
            transactions: Vec::new(),
            chain_metadata: ChainMetadata {
                sapling_tree_size: height,
                orchard_tree_size: height,
            },
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

    #[test]
    fn compact_block_serves_the_stored_domain_block() {
        let w = window(101, 5);
        // In range: the stored CompactBlock (with its composed tree sizes).
        let cb = w.compact_block(to_height(103)).expect("in range");
        assert_eq!(cb.height, 103);
        assert_eq!(cb.chain_metadata.sapling_tree_size, 103);
        // Out of range: None.
        assert!(w.compact_block(to_height(200)).is_none());
    }
}
