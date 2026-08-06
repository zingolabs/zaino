//! Persistent best-chain vector — block-height-indexed with structural sharing.
//!
//! Wraps `im::Vector` for O(1) clones. Ingestion pushes to the back, freeze pops
//! from the front, reorgs truncate + append. All are O(log n) with structural
//! sharing.
//!
//! **Vendored** from Hahn's `zaino-store::chain` (PR #1378, Lean-verified reorg
//! machinery), adapted narrowly:
//! - the element is our [`PreIndexCompactBlock`] (carries transparent I/O, so
//!   the window can re-derive spend/address facts) instead of his opaque
//!   `Block { data: Vec<u8> }`;
//! - height indexing stays raw `u32` (his `Height` alias); the `NfsSpine` port
//!   converts to/from the domain [`zaino_core::Height`] at the boundary.
//!
//! Reconcile with `zaino-store` when #1378 lands. See [[project_zallet_fit_mirror]]
//! for the mirror-then-reconcile pattern.
//!
// Vendored core, not yet consumed — the `NfsSpine` impl and the follow loop wire
// it in the following commits. Remove this once `spine`/`window` use it.
#![allow(dead_code)]

use std::sync::Arc;

use zaino_core::{BlockHash, PreIndexCompactBlock};

/// A block in the window: the source-side compact block, which carries the
/// header (`hash`/`prev_hash`/`height`) the chain indexes by and the transparent
/// I/O the facets scan.
type Block = PreIndexCompactBlock;

/// Persistent chain: best chain in height order.
///
/// `chain[i]` is the block at height `chain.start + i`. The chain is dense:
/// every height from `start` to `start + len - 1` has exactly one block.
///
/// Structural sharing via `Arc<im::Vector<Block>>`. `Clone` is O(1) — this is
/// how a snapshot pins the window; push/pop/truncate/append allocate only the
/// changed spine.
#[derive(Debug, Clone)]
pub(crate) struct Chain {
    /// The lowest height covered by this chain.
    pub(crate) start: u32,
    inner: Arc<im::Vector<Block>>,
}

impl Chain {
    /// Create an empty chain starting at `start`.
    pub(crate) fn new(start: u32) -> Self {
        Self {
            start,
            inner: Arc::new(im::Vector::new()),
        }
    }

    /// Look up the block at `height`. Returns `None` if out of range.
    pub(crate) fn get(&self, height: u32) -> Option<&Block> {
        if height < self.start {
            return None;
        }
        let idx = (height - self.start) as usize;
        self.inner.get(idx)
    }

    /// The last (highest-height) block in the chain.
    pub(crate) fn last(&self) -> Option<&Block> {
        self.inner.last()
    }

    /// The tip hash (hash of the highest block).
    pub(crate) fn tip_hash(&self) -> Option<BlockHash> {
        self.inner.last().map(|b| b.hash)
    }

    /// The tip height (`start + len - 1`), or `None` if empty.
    pub(crate) fn tip_height(&self) -> Option<u32> {
        self.last().map(|b| b.height)
    }

    /// Push a block to the back (tip extension). O(log n) structural sharing.
    pub(crate) fn push_back(&self, block: Block) -> Self {
        let mut new_vec = (*self.inner).clone();
        new_vec.push_back(block);
        Self {
            start: self.start,
            inner: Arc::new(new_vec),
        }
    }

    /// Pop from the front (freeze). O(log n) structural sharing.
    /// Returns `None` if empty, otherwise `Some((block, new_chain))`.
    pub(crate) fn pop_front(&self) -> Option<(Block, Self)> {
        if self.inner.is_empty() {
            return None;
        }
        let block = self.inner[0].clone();
        let mut new_vec = (*self.inner).clone();
        new_vec.pop_front();
        Some((
            block,
            Self {
                start: self.start + 1,
                inner: Arc::new(new_vec),
            },
        ))
    }

    /// Number of blocks in the chain.
    pub(crate) fn len(&self) -> usize {
        self.inner.len()
    }

    /// Whether the chain is empty.
    pub(crate) fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Truncate entries from `trim_from` (inclusive) onward.
    /// Returns a new chain covering `[start, trim_from - 1]`.
    /// If `trim_from <= start`, returns an empty chain positioned at `start`.
    pub(crate) fn truncate_from_incl(&self, trim_from: u32) -> Self {
        if trim_from <= self.start {
            return Self::new(self.start);
        }
        let keep = (trim_from - self.start) as usize;
        if keep >= self.inner.len() {
            return self.clone();
        }
        let new_inner: im::Vector<Block> = self.inner.iter().take(keep).cloned().collect();
        Self {
            start: self.start,
            inner: Arc::new(new_inner),
        }
    }

    /// Append a fragment to the back. O(log n) structural sharing.
    /// The fragment must be non-empty and form a valid chain extension.
    pub(crate) fn append(&self, fragment: &im::Vector<Block>) -> Self {
        if fragment.is_empty() {
            return self.clone();
        }
        let mut new_vec = (*self.inner).clone();
        new_vec.append(fragment.clone());
        Self {
            start: self.start,
            inner: Arc::new(new_vec),
        }
    }

    /// Truncate at `trim_from` (inclusive), then append `fragment` — apply a
    /// reorg. Keeps `[start, trim_from - 1]` and appends `fragment` starting at
    /// `trim_from`. For an empty chain, `trim_from` must be ≤ `start`;
    /// `trim_from = 0` means discard everything and start fresh.
    ///
    /// The caller must ensure `fragment[0].prev_hash` matches the block at
    /// `trim_from - 1` (in the finalised state when `trim_from <= start`, or in
    /// the chain when `trim_from > start`).
    pub(crate) fn add_fragment(
        &self,
        trim_from: u32,
        fragment: impl Into<im::Vector<Block>>,
    ) -> Self {
        let fragment = fragment.into();
        self.truncate_from_incl(trim_from).append(&fragment)
    }

    /// Iterate over (height, &block) pairs.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (u32, &Block)> {
        let start = self.start;
        self.inner
            .iter()
            .enumerate()
            .map(move |(i, b)| (start + i as u32, b))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal window block distinguished by its header `(hash, prev_hash,
    /// height)`; no transactions (facet re-derivation is exercised elsewhere).
    fn test_block(height: u32, prev_hash: BlockHash) -> Block {
        PreIndexCompactBlock {
            hash: BlockHash::from([height as u8; 32]),
            prev_hash,
            height,
            time: 0,
            bits: 0,
            transactions: Vec::new(),
        }
    }

    fn hash(tag: u8) -> BlockHash {
        BlockHash::from([tag; 32])
    }

    #[test]
    fn chain_push_and_get() {
        let c = Chain::new(0);
        let c = c.push_back(test_block(0, hash(0)));
        assert_eq!(c.get(0).expect("h0").height, 0);
        assert!(c.get(1).is_none());

        let c2 = c.push_back(test_block(1, hash(0)));
        // Old unchanged.
        assert!(c.get(1).is_none());
        // New has both.
        assert_eq!(c2.get(0).expect("h0").height, 0);
        assert_eq!(c2.get(1).expect("h1").height, 1);
    }

    #[test]
    fn chain_pop_front() {
        let c = Chain::new(0)
            .push_back(test_block(0, hash(0)))
            .push_back(test_block(1, hash(0)));

        let (block, c2) = c.pop_front().expect("non-empty");
        assert_eq!(block.height, 0);
        assert_eq!(c2.start, 1);
        assert!(c2.get(0).is_none());
        assert_eq!(c2.get(1).expect("h1").height, 1);
    }

    #[test]
    fn chain_truncate_from_incl() {
        let c = Chain::new(0)
            .push_back(test_block(0, hash(0)))
            .push_back(test_block(1, hash(0)))
            .push_back(test_block(2, hash(1)));

        // trim_from = 2: keep [0, 1], remove [2..].
        let c_t = c.truncate_from_incl(2);
        assert_eq!(c_t.len(), 2);
        assert_eq!(c_t.get(1).expect("h1").height, 1);
        assert!(c_t.get(2).is_none());
        // Original unchanged.
        assert!(c.get(2).is_some());

        // trim_from <= start: empty chain.
        let c_empty = c.truncate_from_incl(0);
        assert_eq!(c_empty.len(), 0);
        assert_eq!(c_empty.start, 0);
    }

    #[test]
    fn chain_add_fragment_tip_extend() {
        let b0 = test_block(0, hash(0));
        let b1 = test_block(1, b0.hash);
        let c = Chain::new(0).push_back(b0);

        let fragment: im::Vector<Block> = std::iter::once(b1).collect();
        let c2 = c.add_fragment(1, fragment);
        assert_eq!(c2.last().expect("tip").height, 1);
    }

    #[test]
    fn chain_add_fragment_reorg() {
        let b0 = test_block(0, hash(0));
        let b1 = test_block(1, b0.hash);
        let b2 = test_block(2, b1.hash);
        let b1_hash = b1.hash;
        let b2_hash = b2.hash;
        let c = Chain::new(0).push_back(b0).push_back(b1).push_back(b2);

        // Reorg at height 1: trim from 2, replace height 2 with alternative b2a.
        let b2a = test_block(2, b1_hash);
        // Distinguish b2a from b2 by prev is the same but re-tag the hash.
        let b2a = PreIndexCompactBlock {
            hash: hash(0xEE),
            ..b2a
        };
        let b2a_hash = b2a.hash;
        assert_ne!(b2a_hash, b2_hash);
        let fragment: im::Vector<Block> = std::iter::once(b2a).collect();
        let c2 = c.add_fragment(2, fragment);
        assert_eq!(c2.len(), 3);
        // Original unchanged.
        assert_eq!(c.last().expect("tip").hash, b2_hash);
        // New has the alternative at height 2.
        assert_eq!(c2.get(2).expect("h2").hash, b2a_hash);
    }

    #[test]
    fn chain_structural_sharing_pins_a_snapshot() {
        let b0 = test_block(0, hash(0));
        let b1 = test_block(1, b0.hash);
        let b1_hash = b1.hash;
        let c = Chain::new(0).push_back(b0).push_back(b1);

        // Snapshot before mutation (an O(1) clone).
        let snap = c.clone();

        let c2 = c.push_back(test_block(2, b1_hash));

        // The pinned snapshot still only has heights 0,1.
        assert_eq!(snap.len(), 2);
        assert!(snap.get(2).is_none());
        // The advanced chain has all three.
        assert_eq!(c2.len(), 3);
        assert!(c2.get(2).is_some());
    }

    #[test]
    fn chain_last_and_tip() {
        let b0 = test_block(0, hash(0));
        let b1 = test_block(1, b0.hash);
        let b1_hash = b1.hash;
        let c = Chain::new(0).push_back(b0).push_back(b1);

        assert_eq!(c.tip_height(), Some(1));
        assert_eq!(c.tip_hash(), Some(b1_hash));
    }
}
