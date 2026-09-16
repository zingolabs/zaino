//! A block as a chain view reports it.

use zaino_chain_store::FrozenBlock;
use zaino_chain_store::StoredBlock;
use zaino_chain_store::StoredTx;
use zaino_primitives::types::{Block, PreIndexCompactTx, SignedZatoshis};
use zaino_primitives::types::{BlockHeader, BlockRef, ChainWork, TreeRoots};

/// A block, from whichever provider held it.
///
/// **Seperate from [`StoredBlock`]**:
///
/// [`StoredBlock::chainwork`] is a plain [`ChainWork`], because a store always
/// knows it: the store's coverage runs from genesis, so the cumulative sum is
/// available for every block it holds. A chain view has no such guarantee — it
/// answers from providers that individually cannot know it — so here the field
/// is an [`Option`].
///
/// Reusing `StoredBlock` would have meant either changing the store's type to
/// accommodate a consumer's limitation, or encoding "unknown" as a magic value
/// inside a field typed as if it were always known. The field types are shared,
/// so this is a different assembly of the same vocabulary rather than a second
/// copy of it.
#[derive(Debug, Clone)]
pub struct ChainBlock {
    /// The block's header.
    pub header: BlockHeader,
    /// Per-transaction indexed data, in block order.
    pub transactions: Vec<StoredTx>,
    /// Commitment tree roots and sizes after this block is applied.
    pub tree_roots: TreeRoots,
    /// Cumulative work from genesis to this block, when it can be known.
    ///
    /// Chainwork is *cumulative*: it is the total work of every block from
    /// genesis to this one. Knowing it therefore requires an unbroken chain
    /// below this block, not merely this block.
    pub chainwork: Option<ChainWork>,
}

impl ChainBlock {
    /// This block's height and hash.
    pub fn reference(&self) -> BlockRef {
        BlockRef {
            height: self.header.height,
            hash: self.header.hash,
        }
    }

    /// A block the finalised store answered for.
    pub(crate) fn from_stored(block: StoredBlock) -> Self {
        Self {
            header: block.header,
            transactions: block.transactions,
            tree_roots: block.tree_roots,
            chainwork: Some(block.chainwork),
        }
    }

    /// A block the chain head or the validator answered for.
    pub(crate) fn from_parsed(
        block: &Block,
        tree_roots: TreeRoots,
        chainwork: Option<ChainWork>,
    ) -> Self {
        Self {
            header: block.header.clone(),
            transactions: stored_txs(block),
            tree_roots,
            chainwork,
        }
    }
}

/// A block on its way into the finalised store.
pub(crate) fn frozen_block(block: &Block, tree_roots: TreeRoots) -> FrozenBlock {
    FrozenBlock {
        header: block.header.clone(),
        transactions: stored_txs(block),
        tree_roots,
    }
}

/// Every transaction in a block, as an index holds it.
fn stored_txs(block: &Block) -> Vec<StoredTx> {
    block.transactions.iter().map(stored_tx).collect()
}

/// The indexed projection of a parsed transaction.
fn stored_tx(tx: &zaino_primitives::types::Transaction) -> StoredTx {
    StoredTx {
        compact: PreIndexCompactTx::from(tx),
        sapling_value: pool_value(
            tx.sapling.spends.is_empty() && tx.sapling.outputs.is_empty(),
            tx.sapling.value_balance,
        ),
        orchard_value: pool_value(tx.orchard.actions.is_empty(), tx.orchard.value_balance),
        ironwood_value: pool_value(tx.ironwood.actions.is_empty(), tx.ironwood.value_balance),
    }
}

/// A pool's balance, or `None` where the transaction does not use the pool.
fn pool_value(pool_is_empty: bool, balance: SignedZatoshis) -> Option<SignedZatoshis> {
    (!pool_is_empty).then_some(balance)
}
