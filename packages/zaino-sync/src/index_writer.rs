//! What an index must do to be driven by [`IndexFollower`](crate::IndexFollower).
//!
//! Sync is `f(old_state, blocks)`. The non-finalized state is that same `f`, applied and not
//! yet finalised — so an index has **two** tips, not one (each a last height, inclusive):
//!
//! - [`applied_height`](IndexWriter::applied_height) — non-finalized, reaches the chain tip
//! - [`finalized_tip`](IndexWriter::finalized_tip) — durable, lags by `finalised_depth`
//!
//! A reorg drops the non-finalized state and re-applies; a restart finds it empty and re-applies.
//! **Same operation**, which matters more than the code it saves: the reorg path is rare and
//! therefore under-tested, and this makes it the path that runs on every boot.
//!
//! Nothing here knows about storage. The index owns its files, its cumulative state, its write
//! cadence and both its heights; the harness owns the loop.
//!
//! See `docs/design/non-finalized-state.md`.

use std::{future::Future, sync::Arc};

use zaino_primitives::types::{Block, BlockHash, BlockRef, Height};

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

    /// Immutable snapshot of the non-finalized state, pinned by readers.
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

    /// Last durable block, as committed (inclusive; `None` = nothing on disk)
    ///
    /// - Resume point, and what anything downstream gates on; never moves backwards
    /// - Chain identity the next delivered block must link onto (`FollowError::Unlinked`)
    fn finalized_tip(&self) -> Option<BlockRef>;

    /// [`finalized_tip`](Self::finalized_tip)'s height
    fn finalized_height(&self) -> Option<Height> {
        self.finalized_tip().map(|tip| tip.height)
    }

    /// Last applied height, inclusive (`None` = nothing applied): the next
    /// [`apply`](Self::apply) expects the height after it
    ///
    /// Moves backwards only on [`reset`](Self::reset), and only to
    /// [`finalized_height`](Self::finalized_height).
    fn applied_height(&self) -> Option<Height>;

    /// Non-finalized state, for readers to pin. Serving consults this *before* durable state.
    fn view(&self) -> Self::View;

    /// Every delivered block, in order, before the harness stages or applies any of `blocks`
    ///
    /// - `blocks` = a contiguous run of the steps already queued (one block when following the
    ///   tip, up to a batch's bytes in bulk): per-block work batches across it
    /// - Heights at or below [`finalized_height`](Self::finalized_height) arrive here and nowhere
    ///   else
    ///   (the sink feeds every index from the rearmost resume point): skipping them = the default
    fn deliver(
        &mut self,
        blocks: &[Arc<Self::Input>],
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        let _ = blocks;
        async { Ok(()) }
    }

    /// Folds one block into the non-finalized state. Not durable until
    /// [`finalize`](Self::finalize).
    fn apply(
        &mut self,
        block: &Arc<Self::Input>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Prepares `blocks` for durable storage, contiguous and ascending from the height after
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
    /// everything arrives already final and skips the non-finalized state entirely, and only the
    /// reorg window pays for both.
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
    /// moves, the written blocks leave the non-finalized state, downstream is told.
    ///
    /// **Advances [`applied_height`](Self::applied_height) too**, to at least the new
    /// `finalized_height`. `applied < finalized` is incoherent — an index that only moved its
    /// durable tip would fold the next `apply` onto stale carry.
    ///
    /// Every tier change lands here in one step: readers see a written block in exactly one tier
    /// (non-finalized until now, durable after), never both and never neither.
    fn committed(
        &mut self,
        done: Self::Done,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Drops **all** non-finalized state, the reorg move. After this,
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

/// An index whose every delivered block derives one item for another sink
/// ([`IndexFollower::publishing`](crate::IndexFollower::publishing) republishes it)
pub trait Derives: IndexWriter {
    type Item: crate::Weight + Send + Sync + 'static;

    /// One item per block of `blocks`, in order: the run just passed to
    /// [`deliver`](IndexWriter::deliver), durable heights included (a downstream index behind
    /// this one still pairs them)
    fn derive(
        &mut self,
        blocks: &[Arc<Self::Input>],
    ) -> impl Future<Output = Result<Vec<Self::Item>, Self::Error>> + Send;
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
