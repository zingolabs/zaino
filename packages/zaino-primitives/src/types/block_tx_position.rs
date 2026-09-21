//! Where a transaction sits inside a block.

use crate::types::{Height, TxIndex};

/// A transaction's position on the best chain: which block, and where in it.
///
/// Distinct from [`TransactionLocation`](super::TransactionLocation), which
/// answers *which chain* a transaction is on. This answers *where* it is, and
/// only makes sense once that question is already settled.
///
/// Carries a [`Height`] rather than a block reference because it describes a
/// best-chain position, where height names the block on its own. The chain
/// head's equivalent — `ChainHeadTxPosition` — carries a `BlockRef` instead,
/// because it retains competing branches and a height there names several
/// blocks at once. The two are near-duplicates today and are worth reconciling
/// once both halves are read through their own vocabularies; they are separate
/// for now because the difference between them is real.
///
/// `tx_index` is a [`TxIndex`] (`u32`), which is wider than any block can
/// currently hold — the block size limit bounds a block to far fewer
/// transactions than that. A backend is free to store it narrower; that is a
/// property of its encoding, not of the position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockTxPosition {
    /// The block containing the transaction.
    pub height: Height,
    /// The transaction's 0-based index within that block.
    pub tx_index: TxIndex,
}

impl BlockTxPosition {
    /// A position at `height`, `tx_index` transactions in.
    pub fn new(height: Height, tx_index: TxIndex) -> Self {
        Self { height, tx_index }
    }

    /// Whether this position names a block's coinbase transaction.
    ///
    /// The coinbase is always first, so the index alone decides it. Worth
    /// naming because the coinbase is the one transaction whose inputs spend
    /// nothing, and callers walking spends have to skip it.
    pub fn is_coinbase(&self) -> bool {
        self.tx_index == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(height: u32, tx_index: TxIndex) -> BlockTxPosition {
        BlockTxPosition::new(Height::try_from(height).expect("valid height"), tx_index)
    }

    #[test]
    fn the_first_transaction_is_the_coinbase() {
        assert!(at(100, 0).is_coinbase());
        assert!(!at(100, 1).is_coinbase());
    }

    /// Positions sort in chain order: by height, then by index within a block.
    ///
    /// The ordering is derived, so it follows field declaration order. Pinned
    /// because reordering the fields would silently reverse the precedence and
    /// leave anything sorting positions — a range walk, a merge across the
    /// finalised/recent seam — quietly wrong rather than broken.
    #[test]
    fn positions_sort_in_chain_order() {
        let mut positions = vec![at(2, 0), at(1, 5), at(2, 1), at(1, 0)];
        positions.sort();
        assert_eq!(positions, vec![at(1, 0), at(1, 5), at(2, 0), at(2, 1)]);
    }
}
