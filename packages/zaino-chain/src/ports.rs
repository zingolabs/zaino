//! What a chain view offers, split so a bound names what it uses.
//!
//! [`ChainViewSnapshot`] bundles the reads every composition can answer. A
//! consumer needing an optional one names it beside the bundle:
//!
//! ```ignore
//! fn serve(chain: impl ChainViewSnapshot + SpendRead) { .. }
//! ```
//!
//! # Ranges stream; points do not
//!
//! A range read returns a stream of *chunks*. Chunks rather than single blocks
//! because per-item overhead across hundreds of thousands of blocks dominates,
//! and streaming rather than a `Vec` because a wallet syncing the whole chain
//! would otherwise materialise it — with thousands of clients doing so at once,
//! that is the difference between bounded and unbounded memory.
//!
//! The streams are lazy, so a slow client applies backpressure by not polling
//! rather than by filling a buffer. `use<Self>` excludes the `&self` lifetime,
//! making them `'static` so a consumer can move one into a spawned task; an
//! implementation clones what the walk needs, which is cheap here because a
//! snapshot is a handful of `Arc`s.

use core::future::Future;

use futures::Stream;
use tokio::sync::watch;
use zaino_chain_store::{PoolFilter, TxOutSetAccumulator};
use zaino_primitives::types::{
    rpc::ChainTip, AddressBalance, AddressDelta, BlockHash, BlockHeader, BlockRef, ChainStateEpoch,
    CompactBlock, Height, Outpoint, ShieldedPool, SubtreeIndex, SubtreeRoot, TransactionId,
    TransparentAddress, Treestate, Utxo,
};

use crate::block::ChainBlock;
use crate::capability::{ServiceabilityManifest, ServiceableRange};
use crate::error::Result;
use crate::types::{
    BlockId, ChainScope, Locator, RawTransaction, SpendStatus, TransactionLocations,
};

/// A live chain view.
pub trait ChainView: Send + Sync {
    /// The pinned view its reads are answered from.
    type Snapshot: ChainViewSnapshot;

    /// Captures the current view.
    ///
    /// Neither fallible nor awaited: the chain head hands back a published
    /// snapshot infallibly, the watermark is an in-memory value updated on
    /// commit, and a store reader is cheap to clone. Every ingredient is
    /// present when the call is made, so there is nothing to await and no
    /// failure to invent — and with thousands of clients taking snapshots, this
    /// has to stay O(1).
    fn snapshot(&self) -> Self::Snapshot;

    /// Watches which chain state the view is on.
    ///
    /// A `watch` rather than a broadcast: a late subscriber wants the current
    /// state, not the history. The epoch changes on every tip change, reorgs
    /// included, so a consumer reacting to one takes a fresh snapshot and must
    /// not assume the new tip is a child of the old.
    fn subscribe_tip(&self) -> watch::Receiver<ChainStateEpoch>;

    /// What this view offers, and how far.
    fn serviceability(&self) -> ServiceabilityManifest;
}

/// An immutable view of the chain as of one tip.
///
/// Every read observes the chain as of the pinned tip and keeps doing so while
/// any clone lives — across reorgs and across the providers advancing
/// underneath.
pub trait ChainViewSnapshot:
    BlockRead
    + CompactBlockRead
    + TransactionRead
    + TreestateRead
    + ForkReconcile
    + Clone
    + Send
    + Sync
    + 'static
{
    /// The tip this view is pinned to.
    fn tip(&self) -> BlockRef;

    /// The heights this view can answer between.
    fn serviceable_range(&self) -> ServiceableRange;
}

/// Blocks and headers.
pub trait BlockRead: Send + Sync {
    /// The canonical hash at `height`.
    fn block_hash(&self, height: Height) -> impl Future<Output = Result<Option<BlockHash>>> + Send;

    /// The height of `hash`, if it is on the canonical chain.
    ///
    /// `None` for a hash on a retained competing branch as well as one nobody
    /// holds: this answers about the best chain, and reporting a branch height
    /// would say "this block is at height N on the chain you are reading",
    /// which is false. [`ForkReconcile::fork_point`] is the branch question.
    fn block_height(&self, hash: BlockHash) -> impl Future<Output = Result<Option<Height>>> + Send;

    /// The indexed block at `at`.
    fn block(&self, at: BlockId) -> impl Future<Output = Result<Option<ChainBlock>>> + Send;

    /// The header of the block at `at`.
    fn block_header(&self, at: BlockId)
        -> impl Future<Output = Result<Option<BlockHeader>>> + Send;

    /// The consensus bytes of the block at `at`.
    fn raw_block(&self, at: BlockId) -> impl Future<Output = Result<Option<Vec<u8>>>> + Send;

    /// The indexed blocks in `start..=end`, ascending, as a stream of chunks.
    ///
    /// May be answered by several providers at once — the store below its
    /// watermark, the validator across a range it has not built, the chain head
    /// above that — stitched in height order. Truncated at the chain tip, so a
    /// caller asks for what it wants and reads what it got.
    fn stream_blocks(
        &self,
        start: Height,
        end: Height,
    ) -> impl Stream<Item = Result<Vec<ChainBlock>>> + Send + use<Self>;

    /// The consensus bytes of the blocks in `start..=end`, ascending.
    fn stream_raw_blocks(
        &self,
        start: Height,
        end: Height,
    ) -> impl Stream<Item = Result<Vec<Vec<u8>>>> + Send + use<Self>;
}

/// Compact blocks, for wallet sync.
///
/// Separate from [`BlockRead`] because the pool filter is part of the read: it
/// decides which of the store's per-pool data is decoded at all, so a
/// sapling-only wallet does not pay to decode orchard and ironwood on every
/// block of its sync.
pub trait CompactBlockRead: Send + Sync {
    /// The compact block at `height`.
    fn compact_block(
        &self,
        height: Height,
        pools: PoolFilter,
    ) -> impl Future<Output = Result<Option<CompactBlock>>> + Send;

    /// The compact blocks in `start..=end`, ascending, as a stream of chunks.
    ///
    /// The wallet-sync hot path, and the read this crate is shaped around.
    fn stream_compact(
        &self,
        start: Height,
        end: Height,
        pools: PoolFilter,
    ) -> impl Stream<Item = Result<Vec<CompactBlock>>> + Send + use<Self>;
}

/// Transactions, and where they were mined.
pub trait TransactionRead: Send + Sync {
    /// The consensus bytes of `txid`.
    fn raw_transaction(
        &self,
        txid: TransactionId,
    ) -> impl Future<Output = Result<Option<RawTransaction>>> + Send;

    /// Everywhere this view knows `txid` to have been mined.
    ///
    /// A transaction sitting only in the mempool appears nowhere here: a chain
    /// view does not track a mempool, so a consumer distinguishing "unmined"
    /// from "unknown" asks the mempool separately.
    fn transaction_locations(
        &self,
        txid: TransactionId,
    ) -> impl Future<Output = Result<TransactionLocations>> + Send;
}

/// Commitment tree state.
pub trait TreestateRead: Send + Sync {
    /// The commitment trees as of the block at `at`.
    fn treestate(&self, at: BlockId) -> impl Future<Output = Result<Option<Treestate>>> + Send;

    /// Subtree roots for `pool`, from `start_index`, at most `limit`.
    ///
    /// Indexed by subtree completion rather than by height, and the two do not
    /// correspond — a subtree completes when it fills, not on a block boundary.
    fn subtree_roots(
        &self,
        pool: ShieldedPool,
        start_index: SubtreeIndex,
        limit: Option<u16>,
    ) -> impl Future<Output = Result<Vec<SubtreeRoot>>> + Send;
}

/// The shape of the chain, and reconciling a client's view of it.
pub trait ForkReconcile: Send + Sync {
    /// Every tip of the retained graph, canonical and competing.
    fn chain_tips(&self) -> Vec<ChainTip>;

    /// The most recent block in `locator` that is on the canonical chain.
    ///
    /// What a client resyncing after a reorg asks. It offers the hashes it
    /// believes in, most recent first, and gets back the newest one still on
    /// the chain — the point from which to resume. `None` when it recognises
    /// none of them, which means resyncing from further back.
    fn fork_point(
        &self,
        locator: &Locator,
    ) -> impl Future<Output = Result<Option<BlockRef>>> + Send;

    /// The blocks from `from` up to the pinned tip, ascending.
    ///
    /// The companion to [`Self::fork_point`]: having found where a client
    /// diverged, this is what brings it forward.
    fn stream_blocks_to_tip(
        &self,
        from: Height,
    ) -> impl Stream<Item = Result<Vec<ChainBlock>>> + Send + use<Self>;
}

// ***** Optional capabilities *****
//
// Implemented only where the providers can supply them. A composition whose
// store lacks the backing index does not get these impls at all.

/// Transparent address history.
pub trait AddressRead: Send + Sync {
    /// The total transparent balance of these addresses.
    fn address_balance(
        &self,
        addresses: &[TransparentAddress],
    ) -> impl Future<Output = Result<AddressBalance>> + Send;

    /// Every unspent transparent output of these addresses.
    fn address_utxos(
        &self,
        addresses: &[TransparentAddress],
    ) -> impl Future<Output = Result<Vec<Utxo>>> + Send;

    /// The transactions touching these addresses within `start..=end`.
    fn address_txids(
        &self,
        addresses: &[TransparentAddress],
        start: Height,
        end: Height,
    ) -> impl Future<Output = Result<Vec<TransactionId>>> + Send;

    /// The balance changes of these addresses within `start..=end`.
    fn address_deltas(
        &self,
        addresses: &[TransparentAddress],
        start: Height,
        end: Height,
    ) -> impl Future<Output = Result<Vec<AddressDelta>>> + Send;
}

/// What became of transparent outputs.
pub trait SpendRead: Send + Sync {
    /// What became of each outpoint, in the order asked.
    ///
    /// `scope` chooses how far to look: [`ChainScope::Finalised`] answers only
    /// from the store and is reorg-stable; [`ChainScope::FullChain`] includes
    /// the recent window, and refuses if the providers leave a range the
    /// validator cannot fill — it runs no spend index, so an answer spanning
    /// one would be silently incomplete.
    fn outpoint_spenders(
        &self,
        outpoints: &[Outpoint],
        scope: ChainScope,
    ) -> impl Future<Output = Result<Vec<SpendStatus>>> + Send;
}

/// The unspent transparent output set's running totals.
pub trait TxOutSetRead: Send + Sync {
    /// The accumulator as of the finalised watermark.
    ///
    /// A partial fold, not an answer at the tip: a consumer serving
    /// `gettxoutsetinfo` extends it with what the recent window created and
    /// spent.
    fn txout_set(&self) -> impl Future<Output = Result<TxOutSetAccumulator>> + Send;
}
