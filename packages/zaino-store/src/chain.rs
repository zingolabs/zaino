//! Persistent best-chain vector — block-height-indexed with structural sharing.
//!
//! Wraps `im::Vector<Block>` for O(1) clones. Ingestion pushes to the back,
//! freeze pops from the front, reorgs truncate + append. All are O(log n)
//! with structural sharing.

use std::sync::Arc;

use crate::types::{Block, BlockHash, Height};

/// Persistent chain: best chain in height order.
///
/// `chain[i]` is the block at height `chain.start + i`. The chain is dense:
/// every height from `start` to `start + len - 1` has exactly one block.
///
/// Structural sharing via `Arc<im::Vector<Block>>`.
/// `Clone` is O(1); push/pop/truncate/append allocate only the changed spine.
#[derive(Debug, Clone)]
pub(crate) struct Chain {
    /// The lowest height covered by this chain.
    pub(crate) start: Height,
    inner: Arc<im::Vector<Block>>,
}

impl Chain {
    /// Create an empty chain starting at `start`.
    pub fn new(start: Height) -> Self {
        Self {
            start,
            inner: Arc::new(im::Vector::new()),
        }
    }

    /// Look up the block at `height`. Returns `None` if out of range.
    pub fn get(&self, height: Height) -> Option<&Block> {
        if height < self.start {
            return None;
        }
        let idx = (height - self.start) as usize;
        self.inner.get(idx)
    }

    /// The last (highest-height) block in the chain.
    pub fn last(&self) -> Option<&Block> {
        self.inner.last()
    }

    /// The tip hash (hash of the highest block).
    pub fn tip_hash(&self) -> Option<BlockHash> {
        self.inner.last().map(|b| b.hash)
    }

    /// The tip height (`start + len - 1`), or `None` if empty.
    pub fn tip_height(&self) -> Option<Height> {
        self.last().map(|b| b.height)
    }

    /// Push a block to the back (tip extension). O(log n) structural sharing.
    pub fn push_back(&self, block: Block) -> Self {
        let mut new_vec = (*self.inner).clone();
        new_vec.push_back(block);
        Self {
            start: self.start,
            inner: Arc::new(new_vec),
        }
    }

    /// Pop from the front (freeze). O(log n) structural sharing.
    /// Returns `None` if empty, otherwise `Some((block, new_chain))`.
    pub fn pop_front(&self) -> Option<(Block, Self)> {
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
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Whether the chain is empty.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Truncate entries from `trim_from` (inclusive) onward.
    /// Returns a new chain covering `[start, trim_from - 1]`.
    /// If `trim_from <= start`, returns an empty chain positioned at `start`.
    pub fn truncate_from_incl(&self, trim_from: Height) -> Self {
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
    pub fn append(&self, fragment: &im::Vector<Block>) -> Self {
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

    /// Truncate at `trim_from` (inclusive), then append `fragment`.
    /// Keeps `[start, trim_from - 1]` and appends `fragment` starting at
    /// `trim_from`. For an empty chain, `trim_from` must be ≤ `start`;
    /// `trim_from = 0` means discard everything and start fresh.
    ///
    /// The caller must ensure `fragment[0].prev_hash` matches the block at
    /// `trim_from - 1` (in the freezer when `trim_from <= start`, or in
    /// the chain when `trim_from > start`).
    pub fn add_fragment(&self, trim_from: Height, fragment: impl Into<im::Vector<Block>>) -> Self {
        let fragment = fragment.into();
        self.truncate_from_incl(trim_from).append(&fragment)
    }

    /// Iterate over (height, &block) pairs.
    pub fn iter(&self) -> impl Iterator<Item = (Height, &Block)> {
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

    fn test_block(height: Height, prev_hash: BlockHash) -> Block {
        let hash = [height as u8; 32];
        Block {
            height,
            hash,
            prev_hash,
            data: vec![height as u8],
        }
    }

    #[test]
    fn chain_push_and_get() {
        let c = Chain::new(0);
        let b0 = test_block(0, [0u8; 32]);
        let c = c.push_back(b0);
        assert_eq!(c.get(0).unwrap().height, 0);
        assert!(c.get(1).is_none());

        let b1 = test_block(1, [0u8; 32]);
        let c2 = c.push_back(b1);
        // Old unchanged
        assert_eq!(c.get(1), None);
        // New has both
        assert_eq!(c2.get(0).unwrap().height, 0);
        assert_eq!(c2.get(1).unwrap().height, 1);
    }

    #[test]
    fn chain_pop_front() {
        let c = Chain::new(0);
        let b0 = test_block(0, [0u8; 32]);
        let b1 = test_block(1, [0u8; 32]);
        let c = c.push_back(b0).push_back(b1);

        let (block, c2) = c.pop_front().unwrap();
        assert_eq!(block.height, 0);
        assert_eq!(c2.start, 1);
        assert!(c2.get(0).is_none());
        assert_eq!(c2.get(1).unwrap().height, 1);
    }

    #[test]
    fn chain_truncate_from_incl() {
        let c = Chain::new(0);
        let b0 = test_block(0, [0u8; 32]);
        let b1 = test_block(1, [0u8; 32]);
        let b2 = test_block(2, [1u8; 32]);
        let c = c.push_back(b0).push_back(b1).push_back(b2);

        // trim_from = 2: keep [0, 1], remove [2..]
        let c_t = c.truncate_from_incl(2);
        assert_eq!(c_t.len(), 2); // heights 0,1
        assert_eq!(c_t.get(1).unwrap().height, 1);
        assert!(c_t.get(2).is_none());
        // Original unchanged
        assert!(c.get(2).is_some());

        // trim_from <= start: empty chain
        let c_empty = c.truncate_from_incl(0);
        assert_eq!(c_empty.len(), 0);
        assert_eq!(c_empty.start, 0);
    }

    #[test]
    fn chain_add_fragment_tip_extend() {
        let c = Chain::new(0);
        let b0 = test_block(0, [0u8; 32]);
        let b1 = test_block(1, b0.hash);
        let c = c.push_back(b0);

        let fragment: im::Vector<Block> = vec![b1.clone()].into_iter().collect();
        let c2 = c.add_fragment(1, fragment);
        assert_eq!(c2.last().unwrap().height, 1);
    }

    #[test]
    fn chain_add_fragment_reorg() {
        let c = Chain::new(0);
        let b0 = test_block(0, [0u8; 32]);
        let b1 = test_block(1, b0.hash);
        let b2 = test_block(2, b1.hash);
        // Clone b1 and b2 for later assertions — push_back consumes them.
        let b1_clone = b1.clone();
        let b2_clone = b2.clone();
        let c = c.push_back(b0).push_back(b1).push_back(b2);

        // Reorg at height 1: trim from 2, replace height 2 with alternative b2a
        let b2a = test_block(2, b1_clone.hash);
        let b2a_hash = b2a.hash;
        let fragment: im::Vector<Block> = vec![b2a].into_iter().collect();
        let c2 = c.add_fragment(2, fragment);
        assert_eq!(c2.len(), 3); // heights 0,1,2
        // Original unchanged
        assert_eq!(c.last().unwrap().hash, b2_clone.hash);
        // New has alternative at height 2
        assert_eq!(c2.get(2).unwrap().hash, b2a_hash);
    }

    #[test]
    fn chain_structural_sharing() {
        let c = Chain::new(0);
        let b0 = test_block(0, [0u8; 32]);
        let b1 = test_block(1, b0.hash);
        let b1_hash = b1.hash;
        let c = c.push_back(b0).push_back(b1);

        // Snapshot before mutation
        let snap = c.clone();

        let b2 = test_block(2, b1_hash);
        let c2 = c.push_back(b2);

        // Snapshot still only has heights 0,1
        assert_eq!(snap.len(), 2);
        assert!(snap.get(2).is_none());
        // New chain has all three
        assert_eq!(c2.len(), 3);
        assert!(c2.get(2).is_some());
    }

    #[test]
    fn chain_last_and_tip() {
        let c = Chain::new(0);
        let b0 = test_block(0, [0u8; 32]);
        let b1 = test_block(1, b0.hash);
        let b1_hash = b1.hash;
        let c = c.push_back(b0).push_back(b1);

        assert_eq!(c.tip_height(), Some(1));
        assert_eq!(c.tip_hash(), Some(b1_hash));
    }
}
