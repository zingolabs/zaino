//! What an index must do to be driven by [`IndexFollower`](crate::IndexFollower).
//!
//! Sync is `f(old_state, blocks)`. The non-finalised window is that same `f`, applied and not
//! yet finalised — so an index has **two** extents, not one:
//!
//! - [`applied_height`](IndexWriter::applied_height) — pre-commit, reaches the chain tip
//! - [`finalized_height`](IndexWriter::finalized_height) — durable, lags by `finalised_depth`
//!
//! A reorg drops pre-commit and re-applies; a restart finds pre-commit empty and re-applies.
//! **Same operation**, which matters more than the code it saves: the reorg path is rare and
//! therefore under-tested, and this makes it the path that runs on every boot.
//!
//! Nothing here knows about storage. The index owns its files, its cumulative state, its write
//! cadence and both its heights; the harness owns the loop.
//!
//! See `docs/design/precommit-state.md`.

use std::{future::Future, sync::Arc};

use zaino_primitives::types::{Block, BlockHash, Extent, Height};

/// What the harness reads off a delivered item to prove the chain it builds links up
pub trait Linked {
    fn height(&self) -> Height;
    fn hash(&self) -> BlockHash;
    fn prev_hash(&self) -> BlockHash;
}

impl Linked for Block {
    fn height(&self) -> Height {
        self.header().height
    }

    fn hash(&self) -> BlockHash {
        self.header().hash
    }

    fn prev_hash(&self) -> BlockHash {
        self.header().prev_hash
    }
}

/// An index the harness can drive.
///
/// # Serial order
///
/// [`apply`](Self::apply) is called with strictly contiguous, ascending heights from
/// [`applied_height`](Self::applied_height). Every implementation asserts that at the top — a
/// gap or a repeat means the harness is broken, and a silently mis-indexed chain is far worse
/// than a panic.
///
/// # Off the runtime
///
/// These run on a runtime worker. CPU work (fold, encode, project, sort) goes through
/// [`compute`](crate::compute), a blocking syscall (`fsync`, `pwrite`) through
/// [`blocking`](crate::blocking); anything else stalls the fetch feeding every other index.
pub trait IndexWriter: Send + 'static {
    /// The [`IndexerDataSink`](crate::IndexerDataSink) item this index subscribes to
    type Input: Linked + crate::Weight + Send + Sync + 'static;

    /// Immutable snapshot of pre-commit state, pinned by readers.
    ///
    /// Published after every apply, so it must be cheap to clone — a persistent structure
    /// (`imbl`) sharing with the snapshot it came from, never a deep copy.
    type View: Clone + Send + Sync + 'static;

    /// How this index fails: its storage, its encoding, its own invariants.
    type Error: std::error::Error + Send + Sync + 'static;

    /// What a [`finalize`](Self::finalize) write hands back to [`committed`](Self::committed)
    /// (the lent store, plus whatever landing it needs)
    type Done: Send + 'static;

    /// Names the index in logs, status and metrics.
    const NAME: &'static str;

    /// Durable extent: one past the highest height on disk.
    ///
    /// The resume point, and what anything downstream gates on. Never moves backwards.
    fn finalized_height(&self) -> Extent;

    /// Hash of the durable tip (`finalized_height().last()`), as committed; `None` iff empty
    ///
    /// The chain identity the next delivered block must link onto (`FollowError::Unlinked`).
    fn finalized_tip(&self) -> Option<BlockHash>;

    /// Pre-commit extent: the height the next [`apply`](Self::apply) expects.
    ///
    /// Moves backwards only on [`reset`](Self::reset), and only to
    /// [`finalized_height`](Self::finalized_height).
    fn applied_height(&self) -> Extent;

    /// Pre-commit state, for readers to pin. Serving consults this *before* durable state.
    fn view(&self) -> Self::View;

    /// Every delivered block, in order, before the harness stages or applies any of `blocks`
    ///
    /// - `blocks` = a contiguous run of the steps already queued (one block when following the
    ///   tip, up to a batch's bytes in bulk): per-block work batches across it
    /// - Heights inside [`finalized_height`](Self::finalized_height) arrive here and nowhere else
    ///   (the sink feeds every index from the rearmost resume point): skipping them = the default
    /// - An index feeding another sink publishes from here (one item per block, at every stage)
    fn deliver(
        &mut self,
        blocks: &[Arc<Self::Input>],
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        let _ = blocks;
        async { Ok(()) }
    }

    /// Folds one block into pre-commit state. Not durable until
    /// [`finalize`](Self::finalize).
    fn apply(
        &mut self,
        block: &Arc<Self::Input>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Prepares `blocks` for durable storage, contiguous and ascending from
    /// [`finalized_height`](Self::finalized_height); the returned `write` stores them.
    ///
    /// These are final — below `MAX_BLOCK_REORG_HEIGHT`, so no [`reset`](Self::reset) can reach
    /// them — which is what lets durable structures stay append-only. The harness batches to its
    /// byte budget ([`IndexFollower::new`](crate::IndexFollower::new)), so one write is one fsync.
    ///
    /// Two halves, so a write never stalls delivery:
    /// - here, on the follower: CPU work (encode, fold, sort) through [`compute`](crate::compute);
    ///   nothing durable, nothing in-memory dropped
    /// - `write`, on the blocking pool while the follower keeps delivering and applying: the lent
    ///   store's appends, fsyncs and manifest, handed back through [`Done`](Self::Done)
    ///
    /// While a `write` is out, the writer answers every other call without its store (durable
    /// state as of the last [`committed`](Self::committed)); the harness settles one write before
    /// preparing the next, and before [`apply`](Self::apply) at the bulk → tip handoff or
    /// [`reset`](Self::reset).
    ///
    /// A block here need **not** have been through [`apply`](Self::apply): during bulk sync
    /// everything arrives already final and skips pre-commit entirely, and only the reorg
    /// window pays for both.
    fn finalize(
        &mut self,
        blocks: &[Arc<Self::Input>],
    ) -> impl Future<
        Output = Result<
            impl FnOnce() -> Result<Self::Done, Self::Error> + Send + 'static,
            Self::Error,
        >,
    > + Send;

    /// Lands a finished `write`: the store returns, [`finalized_height`](Self::finalized_height)
    /// moves, the written blocks leave pre-commit, downstream is told.
    ///
    /// **Advances [`applied_height`](Self::applied_height) too**, to at least the new
    /// `finalized_height`. `applied < finalized` is incoherent — an index that only moved its
    /// durable extent would fold the next `apply` onto stale carry.
    ///
    /// Every tier change lands here in one step: readers see a written block in exactly one tier
    /// (pre-commit until now, durable after), never both and never neither.
    fn committed(
        &mut self,
        done: Self::Done,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Drops **all** pre-commit state, the reorg move. After this,
    /// `applied_height() == finalized_height()`.
    ///
    /// Takes no height: state only ever moves forward from a durable point, so no index owns a
    /// reverse fold and none can disagree with another about where a fork lands. The winning
    /// branch arrives as ordinary [`apply`](Self::apply) calls from the durable tip, which is
    /// the path a restart already exercises on every boot.
    ///
    /// **Never touches durable state.** The harness finalises `MAX_BLOCK_REORG_HEIGHT` behind
    /// the tip, so nothing a reorg can reach was ever fsynced.
    fn reset(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// [`finalize`](IndexWriter::finalize) → its write → [`committed`](IndexWriter::committed), in
/// one await (callers outside the follower: tests, offline rebuilds)
pub async fn finalize_now<W: IndexWriter>(
    writer: &mut W,
    blocks: &[Arc<W::Input>],
) -> Result<(), W::Error> {
    let write = writer.finalize(blocks).await?;
    let done = crate::blocking(write).await?;
    writer.committed(done).await
}
