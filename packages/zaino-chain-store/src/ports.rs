//! What a chain store asks of a validator, and what it offers a consumer.

//! What a stream-returning port requires of an implementation.
//!
//! Two constraints come from the return type, and both are deliberate:
//!
//! 1. **One stream type per method.** The return is opaque, not boxed, so an
//!    implementation returns a single concrete type — it cannot pick between
//!    `stream::empty()` and a cursor walk at runtime. Fold the branch into the
//!    stream's own state instead, which is where it belongs: an empty answer is
//!    a walk with nothing to walk. This is what buys the caller a stream with
//!    no allocation and no virtual dispatch.
//!
//! 2. **The stream may not borrow the reader** (`use<Self>` excludes the
//!    `&self` lifetime). Load-bearing rather than incidental: a consumer moves
//!    the stream into a spawned task, so it has to be `'static`. Clone or
//!    `Arc` what the walk needs into it.
//!
//! Neither constrains *how* an implementation chunks. Chunk boundaries carry no
//! meaning, so sizing them by bytes rather than by count, or ramping the first
//! few so latency-to-first-byte stays low, is an implementation's business and
//! needs no port change.
//!
//! The stream is `!Unpin` when built from an async block, which is the usual
//! case. A consumer pins it — `std::pin::pin!` on the stack, which costs
//! nothing — and only one that multiplexes several streams through a
//! combinator like `select_all` needs to box.

use core::future::Future;

use futures::Stream;

use tokio::sync::watch;
use zaino_primitives::types::{
    BlockHash, BlockTxPosition, CompactBlock, Height, Outpoint, TransactionId,
};
use zaino_status::StatusType;

use crate::block::{PoolFilter, StoredBlock};
use crate::capability::{StoreCapabilities, StoreSchema, StoreWatermark};
use crate::error::{ChainStoreError, ChainStoreSourceError};
use crate::output::{SpenderRef, StoredTxOut};
use crate::txout_set::TxOutSetAccumulator;

/// Everything a chain store asks of a validator.
///
/// An alias over `zaino-source`, not a new port. It states a requirement of
/// *this* consumer; `zaino-source` should not have to know who its consumers
/// are, so the list lives here rather than there.
///
/// Four questions, and the list is derived from what the implementation
/// actually calls rather than from what a store might plausibly want:
///
/// - the chain tip, to know how far there is to build;
/// - blocks by height, to build from;
/// - blocks by hash, and transactions, which only the passthrough mode needs —
///   a store configured to hold nothing answers reads from the validator, and
///   so asks questions a building store never does;
/// - commitment tree roots, which are not derivable from a block alone.
///
/// Blocks are asked for parsed rather than raw. A store that took bytes would
/// have to parse them itself, duplicating work the source adapter has already
/// done and a parser Zaino would then maintain twice.
///
/// Not `Clone`. A source may own connections and a database handle that must
/// not be duplicated; the runtime shares one behind an `Arc`.
pub trait ChainStoreSource:
    zaino_source::OneShotGetBestBlockHeight
    + zaino_source::OneShotGetBlock
    + zaino_source::OneShotGetBlockByHash
    + zaino_source::OneShotGetCommitmentTreeRoots
    + zaino_source::OneShotGetTransaction
    + Send
    + Sync
    + 'static
{
}

impl<T> ChainStoreSource for T where
    T: zaino_source::OneShotGetBestBlockHeight
        + zaino_source::OneShotGetBlock
        + zaino_source::OneShotGetBlockByHash
        + zaino_source::OneShotGetCommitmentTreeRoots
        + zaino_source::OneShotGetTransaction
        + Send
        + Sync
        + 'static
{
}

/// A handle onto a running chain store.
///
/// Produces readers and reports health. Deliberately offers no way to make the
/// store build, stop, or roll back: that is [`ChainStoreIngest`], which the
/// owner holds and a reader never sees. Observing how a store is faring is not
/// the same as sequencing it.
pub trait ChainStoreService: Clone + Send + Sync + 'static {
    /// The reader this handle produces.
    type Reader: ChainStoreReader;

    /// A read handle. Cheap: readers share the store rather than opening it.
    fn reader(&self) -> Self::Reader;

    /// How the store is faring.
    fn status(&self) -> StatusType;

    /// Watches the finalised watermark.
    ///
    /// A `watch` rather than a broadcast because a late subscriber wants the
    /// current value, not the history of values — and because a consumer that
    /// falls behind should skip to the present rather than replay every
    /// intermediate height.
    ///
    /// This is what lets a composer route by height without polling, and what
    /// stops it re-deriving the seam from a chain tip it read separately.
    fn subscribe_watermark(&self) -> watch::Receiver<StoreWatermark>;
}

/// The reads every chain store offers.
///
/// A store that cannot answer these is not a store: they are how a consumer
/// discovers what range it covers and resolves a height to a block. Everything
/// beyond this is an index a deployment may or may not build, and is a
/// separate trait so that a bound names exactly what its holder uses.
pub trait ChainStoreReader: Clone + Send + Sync + core::fmt::Debug + 'static {
    /// The highest block this store can answer for.
    ///
    /// Infallible and synchronous: it is held in memory and updated on commit,
    /// so bounding a read against it costs nothing. A caller should read it
    /// once and reuse it for a group of related reads — the value moves as the
    /// store builds, and re-reading it part-way through means answering one
    /// question about two different chains.
    fn watermark(&self) -> StoreWatermark;

    /// What this store currently offers.
    ///
    /// Runtime state, not a type-level fact: a store on an older schema lacks
    /// indexes a newer one has, and gains them during a migration.
    fn capabilities(&self) -> StoreCapabilities;

    /// The schema on disk, and whether it is migrating.
    fn schema(&self) -> impl Future<Output = Result<StoreSchema, ChainStoreError>> + Send;

    /// The canonical hash at `height`.
    ///
    /// `None` when the store does not hold that height; a height above the
    /// watermark is [`ChainStoreError::AboveWatermark`] rather than `None`,
    /// because the block probably exists and simply is not finalised yet.
    fn block_hash(
        &self,
        height: Height,
    ) -> impl Future<Output = Result<Option<BlockHash>, ChainStoreError>> + Send;

    /// The height of the block with `hash`.
    ///
    /// The inverse of [`Self::block_hash`], and load-bearing separately from
    /// it: resolving a height to a hash before fetching by hash is what makes
    /// a read reorg-safe, because a hash names one block for ever where a
    /// height names whichever block is currently there.
    fn block_height(
        &self,
        hash: BlockHash,
    ) -> impl Future<Output = Result<Option<Height>, ChainStoreError>> + Send;

    /// How this store is faring, readable from a reader as well as the
    /// service.
    fn status(&self) -> StatusType;
}

/// Reading indexed blocks.
///
/// Optional: a deployment serving only wallet sync builds compact blocks and
/// never materialises these.
pub trait StoredBlockRead: ChainStoreReader {
    /// The blocks in `start..=end`, inclusive and ascending.
    ///
    /// One transaction, one batch. This is the primitive: a single block is
    /// `blocks_chunk(h, h)`, which is why there is no point-read method — one
    /// would either duplicate this or spawn a task to fetch one block.
    ///
    /// A range extending above the watermark is truncated to it rather than
    /// rejected, so a caller merging with the recent window asks both halves
    /// for the same range and lets each answer what it holds. A range whose
    /// start is above its end is [`ChainStoreError::InvalidRange`].
    fn blocks_chunk(
        &self,
        start: Height,
        end: Height,
    ) -> impl Future<Output = Result<Vec<StoredBlock>, ChainStoreError>> + Send;

    /// The blocks in `start..=end` as a stream of chunks.
    ///
    /// For ranges too large to hold at once. The store chooses chunk
    /// boundaries to bound how long it holds a read transaction; they carry no
    /// meaning and a consumer must not read anything into them.
    fn blocks_stream(
        &self,
        start: Height,
        end: Height,
    ) -> impl Future<
        Output = Result<
            impl Stream<Item = Result<Vec<StoredBlock>, ChainStoreError>> + Send + use<Self>,
            ChainStoreError,
        >,
    > + Send;
}

/// Reading compact blocks.
///
/// Separate from [`StoredBlockRead`] because the filter is pushed into the
/// read: it decides which of the store's per-pool data is touched at all. A
/// consumer deriving compact blocks from stored ones would make a sapling-only
/// wallet pay to decode orchard and ironwood data, and the commitment tree
/// roots, for every block of its sync.
pub trait CompactBlockRead: ChainStoreReader {
    /// The compact blocks in `start..=end`, inclusive and ascending.
    ///
    /// One transaction, one batch. A single block is
    /// `compact_chunk(h, h, pools)`.
    fn compact_chunk(
        &self,
        start: Height,
        end: Height,
        pools: PoolFilter,
    ) -> impl Future<Output = Result<Vec<CompactBlock>, ChainStoreError>> + Send;

    /// The compact blocks in `start..=end` as a stream of chunks.
    ///
    /// The wallet-sync hot path.
    fn compact_stream(
        &self,
        start: Height,
        end: Height,
        pools: PoolFilter,
    ) -> impl Future<
        Output = Result<
            impl Stream<Item = Result<Vec<CompactBlock>, ChainStoreError>> + Send + use<Self>,
            ChainStoreError,
        >,
    > + Send;
}

/// Finding transactions, and finding what is at a position.
pub trait TransactionIndex: ChainStoreReader {
    /// Where `txid` was mined, if the store holds it.
    ///
    /// One position, not many: the store indexes the best chain only, and a
    /// transaction appears at most once on it. The recent window can report
    /// several, because it retains competing branches.
    fn tx_position(
        &self,
        txid: &TransactionId,
    ) -> impl Future<Output = Result<Option<BlockTxPosition>, ChainStoreError>> + Send;

    /// Which transaction is at `position`.
    ///
    /// `None` for a position the store does not hold. A miss is a domain
    /// answer, not a failure — asking about a position past the end of a block
    /// is a reasonable question with the answer "nothing".
    fn txid_at(
        &self,
        position: BlockTxPosition,
    ) -> impl Future<Output = Result<Option<TransactionId>, ChainStoreError>> + Send;
}

/// Transparent outputs and what became of them.
pub trait SpentOutputIndex: ChainStoreReader {
    /// Who spent each outpoint, in the order asked.
    ///
    /// Batched because every caller asks about many at once and the store
    /// answers them under one transaction. One call per outpoint would turn a
    /// single indexed read into a round trip each.
    ///
    /// `None` means "not spent *within this store*" — which is not the same as
    /// unspent, because the spending transaction may be in the recent window
    /// or the mempool. Only a consumer that has checked those too can call an
    /// outpoint unspent.
    fn outpoint_spenders(
        &self,
        outpoints: &[Outpoint],
    ) -> impl Future<Output = Result<Vec<Option<SpenderRef>>, ChainStoreError>> + Send;

    /// What each outpoint refers to, in the order asked.
    ///
    /// `None` when the store does not hold the creating transaction. Batched
    /// for the same reason as [`Self::outpoint_spenders`]: resolving the
    /// inputs of recent blocks against finalised outputs asks about every
    /// input of every block at once.
    fn previous_outputs(
        &self,
        outpoints: &[Outpoint],
    ) -> impl Future<Output = Result<Vec<Option<StoredTxOut>>, ChainStoreError>> + Send;

    /// The output at `outpoint` if it exists and this store has not seen it
    /// spent.
    ///
    /// A first-class question rather than two: composed from
    /// [`Self::previous_outputs`] and [`Self::outpoint_spenders`] it costs two
    /// reads across two indexes, and the caller has to know that "exists" and
    /// "unspent" are different lookups. As with `outpoint_spenders`, unspent
    /// here means unspent *as far as this store knows*.
    fn unspent_output(
        &self,
        outpoint: Outpoint,
    ) -> impl Future<Output = Result<Option<StoredTxOut>, ChainStoreError>> + Send;

    /// The transparent outputs of the transaction at `position`.
    ///
    /// `None` when the store does not hold that position. Needed to decide how
    /// many of a transaction's outputs remain unspent, which is what the
    /// txout-set's transaction count is counting.
    fn transparent_outputs(
        &self,
        position: BlockTxPosition,
    ) -> impl Future<Output = Result<Option<Vec<StoredTxOut>>, ChainStoreError>> + Send;
}

/// The store's running totals over the unspent transparent output set.
pub trait TxOutSetIndex: ChainStoreReader {
    /// The accumulator as of the watermark.
    ///
    /// A partial fold, not an answer: it describes the set up to the finalised
    /// tip, and a consumer serving `gettxoutsetinfo` at the chain tip extends
    /// it with what the recent window has created and spent. The commitment is
    /// defined in [`crate::txout_set`] precisely so that extension produces
    /// the same value the store would have.
    fn txout_set(
        &self,
    ) -> impl Future<Output = Result<TxOutSetAccumulator, ChainStoreError>> + Send;
}

/// Transparent address history within the finalised range.
///
/// Declared and feature-gated. The chain head declares the matching half
/// (`ChainHeadTransparentHistoryService`), and a complete answer is the two
/// merged — neither alone is an address's history.
#[cfg(feature = "transparent_address_history_experimental")]
pub trait TransparentHistoryIndex: ChainStoreReader {
    /// What happened to `query`'s addresses within its height range.
    ///
    /// Range-bounded, always. The store answers for heights up to the
    /// watermark and the recent window answers above it; an unbounded answer
    /// would overlap whatever the caller merges it with, and double-count
    /// every output in the overlap.
    fn address_effects(
        &self,
        query: &crate::transparent::TransparentHistoryQuery,
    ) -> impl Future<Output = Result<crate::transparent::StoreAddressEffects, ChainStoreError>> + Send;
}

/// Driving a chain store forward.
///
/// Held by whoever owns the store, never handed out with a reader. A consumer
/// that could rewind the store it is reading from could invalidate its own
/// answers mid-query.
pub trait ChainStoreIngest: Send + Sync {
    /// Builds up to and including `target`.
    ///
    /// Idempotent and single-flighted: calling it while a build is running is
    /// a no-op rather than a second build, because two writers contending on
    /// one database multiply memory rather than throughput.
    ///
    /// Takes no source. The store owns the validator it was built with, so a
    /// consumer cannot point it at a different chain part-way through.
    fn build_to(
        &self,
        target: Height,
    ) -> impl Future<Output = Result<(), ChainStoreSourceError>> + Send;

    /// Discards everything above `height`.
    ///
    /// The store's history is append-only in normal operation — a reorg deep
    /// enough to affect it is outside the window the chain head covers — so
    /// this is a repair path, not part of following the chain.
    fn rewind_to(&self, height: Height)
        -> impl Future<Output = Result<(), ChainStoreError>> + Send;

    /// Resolves once the store has finished building to its target.
    fn wait_until_built(&self) -> impl Future<Output = ()> + Send;

    /// Stops the store and releases its backend.
    fn shutdown(&self) -> impl Future<Output = Result<(), ChainStoreError>> + Send;
}

/// Accepting blocks that have fallen below the reorg seam.
///
/// Optional, and an optimisation rather than a mechanism: it spares the store
/// re-fetching blocks a composer already has. [`ChainStoreIngest::build_to`]
/// remains the authority, and a store that never receives a frozen block must
/// still reach the same state.
///
/// Takes [`StoredBlock`], not the chain head's block type — this crate does
/// not depend on the chain head, and must not. Converting is the composer's
/// job, and it is not a formality: the chain head measures work from its own
/// anchor, so a composer must rebase that to absolute chainwork before handing
/// a block over. Writing an anchor-relative value would put wrong chainwork on
/// disk.
///
/// # The stream this consumes is not reliable
///
/// Frozen blocks arrive best-effort: a subscriber that falls behind loses
/// some, a restart loses everything in flight, and a reorg that lowers the tip
/// and re-advances can deliver the same heights twice with different hashes.
/// An implementation must therefore be idempotent on `(height, hash)` and must
/// not assume it receives a contiguous run. This is why the source-driven
/// build path cannot be removed.
pub trait ChainStoreFreezeSink: Send + Sync {
    /// Ingests blocks that are now beyond reorg.
    ///
    /// Takes a slice rather than one block: the store writes a batch under one
    /// transaction with its index entries sorted, which turns random-keyed
    /// writes into a sequential sweep. One call per block gives up that, and
    /// pays a transaction and a durability barrier for each.
    fn freeze(
        &self,
        blocks: &[StoredBlock],
    ) -> impl Future<Output = Result<(), ChainStoreError>> + Send;
}

#[cfg(test)]
mod tests {
    use super::ChainStoreSource;

    /// A real validator satisfies the source bound.
    ///
    /// The bound is a list of questions, and nothing else checks that some
    /// adapter can actually answer all of them — a port naming a capability no
    /// source provides would compile perfectly well and fail at wiring time.
    #[test]
    fn the_zebra_validator_satisfies_the_source_bound() {
        fn assert_satisfied<T: ChainStoreSource>() {}
        assert_satisfied::<zaino_source_zebra::ZebraValidator>();
    }
}
