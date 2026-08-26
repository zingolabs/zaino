//! What a published ChainHead view can answer, and the vocabulary those
//! answers are expressed in.
//!
//! A snapshot is an immutable view of the block graph. Every query here is a
//! pure function of one snapshot, so a caller that captures one and asks it
//! several questions gets answers from a single coherent view of the chain —
//! even if the chain reorganised in between.
//!
//! # Capability, not shape
//!
//! [`ChainHeadSnapshot`] is a trait, and deliberately says nothing about how
//! the graph is stored. The collections behind it are an implementation
//! decision belonging to whichever runtime publishes the snapshot: a map-backed
//! graph and a persistent-structure one answer these questions identically, and
//! replacing one with the other must be invisible here and to every consumer.
//!
//! That is also why the queries live on this trait rather than on the runtime
//! handle that produces snapshots. A consumer holding a snapshot already has
//! everything it needs to interrogate it, and each capability is then defined
//! in exactly one place.

use zaino_primitives::types::{
    rpc::ChainTip, BlockHash, BlockRef, ChainStateEpoch, Height, Outpoint, TransactionId, TxIndex,
};

use crate::{block::ChainHeadBlock, error::ChainHeadError};

/// Where a transaction sits in the ChainHead graph.
///
/// Distinct from [`zaino_primitives::types::TransactionLocation`], which
/// answers "which chain is it on" — this answers "which block, and where in
/// it". [`BlockRef`] rather than a bare height because a height alone does not
/// name a block once competing branches are retained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainHeadTxPosition {
    /// The block containing the transaction.
    pub block: BlockRef,
    /// The transaction's index within that block.
    pub tx_index: TxIndex,
}

/// Every place ChainHead knows a transaction to appear.
///
/// The same transaction can sit on the canonical chain and on one or more
/// retained competing branches at once — a reorg does not remove it from the
/// branch it was mined on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ChainHeadTransactionLocations {
    /// Its position on the canonical chain, if it is on it.
    pub best_chain: Option<ChainHeadTxPosition>,
    /// Its positions on retained competing branches.
    pub non_best_chain: Vec<ChainHeadTxPosition>,
}

/// The transaction that spent an outpoint, and where it sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpenderLocation {
    /// The block containing the spending transaction.
    pub block: BlockRef,
    /// The spending transaction.
    pub txid: TransactionId,
    /// The spending transaction's index within that block.
    pub tx_index: TxIndex,
}

/// The blocks a range or window query yields, ascending.
///
/// A named type rather than an anonymous `impl Iterator` because it is part of
/// the port's vocabulary: a consumer can name what it holds, and every
/// implementation yields the same thing regardless of how its graph is stored.
/// Boxing costs one allocation per query, against a walk the caller is about to
/// do anyway — and it keeps [`ChainHeadSnapshot`] object-safe, so a consumer
/// that would rather hold `Arc<dyn ChainHeadSnapshot>` than name a concrete
/// type can.
pub struct ChainHeadBlockIter<'a>(Box<dyn Iterator<Item = &'a ChainHeadBlock> + Send + 'a>);

impl<'a> ChainHeadBlockIter<'a> {
    /// Wraps any ascending iterator over retained blocks.
    pub fn new(inner: impl Iterator<Item = &'a ChainHeadBlock> + Send + 'a) -> Self {
        Self(Box::new(inner))
    }
}

impl<'a> Iterator for ChainHeadBlockIter<'a> {
    type Item = &'a ChainHeadBlock;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next()
    }
}

/// An immutable view of the ChainHead block graph.
///
/// The canonical chain and the competing branches retained alongside it are
/// both visible: [`block_by_hash`](Self::block_by_hash) finds either, while
/// [`best_block_by_height`](Self::best_block_by_height) and
/// [`is_on_best_chain`](Self::is_on_best_chain) are what distinguish them.
pub trait ChainHeadSnapshot: Send + Sync + 'static {
    /// The canonical tip of this view.
    fn best_tip(&self) -> BlockRef;

    /// Which chain state this view represents.
    ///
    /// On the snapshot rather than only on the runtime handle because a
    /// consumer gating on chain state — the mempool's coherence layer is the
    /// one that does — needs the epoch *of the view it is holding*. Reading it
    /// from the handle instead would compare against whatever the chain head
    /// has since published, which is the race the epoch exists to close.
    fn epoch(&self) -> ChainStateEpoch;

    /// The block with this hash, canonical or competing.
    fn block_by_hash(&self, hash: &BlockHash) -> Option<&ChainHeadBlock>;

    /// The canonical block at this height.
    ///
    /// `None` for a height outside the retained window as well as for one with
    /// no canonical block, so a caller routing across the finalised boundary
    /// treats both the same way: ask somewhere else.
    fn best_block_by_height(&self, height: Height) -> Option<&ChainHeadBlock>;

    /// Whether this block is the canonical one at its height.
    ///
    /// Both halves of the reference matter: a block whose height is canonical
    /// but whose hash is not is precisely a competing block.
    fn is_on_best_chain(&self, block: BlockRef) -> bool;

    /// The first canonical ancestor of this block.
    ///
    /// For a canonical block that is the block itself; for a competing block it
    /// is the fork point. `None` when the hash is not retained, or when the
    /// walk leaves the window before reaching the canonical chain — ChainHead
    /// does not claim to know branches rooted below what it retains.
    fn find_fork_point(&self, hash: &BlockHash) -> Option<BlockRef>;

    /// Every tip of the retained graph, canonical and competing, in
    /// `getchaintips` order.
    fn chain_tips(&self) -> Vec<ChainTip>;

    /// Every canonical block in the window, ascending.
    ///
    /// Distinct from [`best_chain_blocks`](Self::best_chain_blocks), which
    /// answers about a range a caller already has in mind: this one is for a
    /// caller that wants the window itself and does not know where it starts.
    /// Asking for the range `0..=tip` instead would make the caller probe
    /// every height below the retention floor to discover it is empty.
    fn best_chain(&self) -> ChainHeadBlockIter<'_>;

    /// The canonical blocks in `start..=end`, ascending.
    ///
    /// Borrows from the snapshot rather than materialising a `Vec`: a range
    /// query over the window is a read, and the caller already holds the
    /// snapshot keeping the blocks alive. Heights with no canonical block are
    /// skipped, so a caller needing a contiguous range checks the count.
    fn best_chain_blocks(
        &self,
        start: Height,
        end: Height,
    ) -> Result<ChainHeadBlockIter<'_>, ChainHeadError>;
}

/// A shared snapshot answers exactly as the snapshot does.
///
/// Snapshots are published behind an [`Arc`](std::sync::Arc) and consumers hold
/// them that way, so without this every call site would have to deref before
/// asking a question.
impl<T: ChainHeadSnapshot> ChainHeadSnapshot for std::sync::Arc<T> {
    fn best_tip(&self) -> BlockRef {
        self.as_ref().best_tip()
    }

    fn epoch(&self) -> ChainStateEpoch {
        self.as_ref().epoch()
    }

    fn block_by_hash(&self, hash: &BlockHash) -> Option<&ChainHeadBlock> {
        self.as_ref().block_by_hash(hash)
    }

    fn best_block_by_height(&self, height: Height) -> Option<&ChainHeadBlock> {
        self.as_ref().best_block_by_height(height)
    }

    fn is_on_best_chain(&self, block: BlockRef) -> bool {
        self.as_ref().is_on_best_chain(block)
    }

    fn find_fork_point(&self, hash: &BlockHash) -> Option<BlockRef> {
        self.as_ref().find_fork_point(hash)
    }

    fn chain_tips(&self) -> Vec<ChainTip> {
        self.as_ref().chain_tips()
    }

    fn best_chain(&self) -> ChainHeadBlockIter<'_> {
        self.as_ref().best_chain()
    }

    fn best_chain_blocks(
        &self,
        start: Height,
        end: Height,
    ) -> Result<ChainHeadBlockIter<'_>, ChainHeadError> {
        self.as_ref().best_chain_blocks(start, end)
    }
}

impl<T: ChainHeadTransactionService> ChainHeadTransactionService for std::sync::Arc<T> {
    fn transaction_locations(&self, txid: &TransactionId) -> ChainHeadTransactionLocations {
        self.as_ref().transaction_locations(txid)
    }

    fn outpoint_spenders(&self, outpoints: &[Outpoint]) -> Vec<Option<SpenderLocation>> {
        self.as_ref().outpoint_spenders(outpoints)
    }
}

/// Transaction facts derivable from a snapshot's blocks.
///
/// Separate from [`ChainHeadSnapshot`] so a consumer's bound names only what it
/// uses. ChainHead answers only about what it retains; complete transaction
/// status — which needs the finalised state and the mempool too — is composed
/// by the consumer.
pub trait ChainHeadTransactionService: ChainHeadSnapshot {
    /// Every place this transaction appears in the retained graph.
    ///
    /// Total: a transaction that appears nowhere is an empty result, not a
    /// failure.
    fn transaction_locations(&self, txid: &TransactionId) -> ChainHeadTransactionLocations;

    /// For each outpoint, the canonical transaction that spent it.
    ///
    /// Output ordering matches input ordering. `None` means "not spent within
    /// ChainHead", which is not the same as unspent — the finalised state holds
    /// the rest of the chain.
    fn outpoint_spenders(&self, outpoints: &[Outpoint]) -> Vec<Option<SpenderLocation>>;
}

/// Transparent-address effects derivable from a snapshot's blocks.
///
/// **Declared, not implemented.** Nothing implements this trait and no consumer
/// is wired to it. See [`crate::transparent`] for why the boundary is drawn
/// where it is.
#[cfg(feature = "transparent_address_history_experimental")]
pub trait ChainHeadTransparentHistoryService: ChainHeadSnapshot {
    /// The address effects this snapshot can account for.
    fn address_effects(
        &self,
        query: &crate::transparent::TransparentHistoryQuery,
    ) -> Result<crate::transparent::ChainHeadAddressEffects, ChainHeadError>;
}
