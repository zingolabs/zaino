//! The composed, pinned view served across the FS⊕NFS seam.

use futures::stream::{self, BoxStream, StreamExt};

use zaino_core::{
    BlockId, BlockRef, Capability, CompactBlock, Height, HeightRange, ServiceableRange,
};
use zaino_service::error::{BlockReadError, ReadError};
use zaino_service::{ChainSegment, CompactBlockRead, Snapshot};

/// A pinned, reorg-coherent view over the composed chain — the finalised store
/// segment `F` and the non-finalised head segment `N`, captured together so the
/// seam watermark and the volatile window agree.
///
/// # The seam, as a relation over heights
///
/// With `watermark = w` (the FS's finalised tip; `None` when the FS is empty),
/// `floor = f`, and `tip = t` (the NFS window bounds):
///
/// ```text
/// FS      = [genesis, w]          (empty when w = None)
/// NFS     = [f, t]                (empty when the head holds nothing)
/// served  = FS ∪ NFS
/// gap     = (w, f)                → NotServiceable   (the initial-build gap)
/// above   = (t, ∞)                → Ok(None)
/// ```
///
/// A height in `FS` reads durable finalised state; a height in `NFS` reads the
/// volatile window; the `gap` is on-chain but held by neither side (the FS is
/// still building up toward the NFS floor); above the tip there is no block.
///
/// Both sides are named only through the shared `zaino-service` ports — each is
/// a [`ChainSegment`] (its coverage names the seam bounds) and a
/// [`CompactBlockRead`] (its by-height/by-hash reads). Only these two capability
/// families are composed here, the reads compact-block serving needs; other
/// reads are named separately or passed through.
#[derive(Clone)]
pub struct ChainViewSnapshot<F, N> {
    /// The finalised store segment, pinned at capture. Serves `[genesis, w]`.
    fs: F,
    /// The non-finalised head segment, captured together with `fs`. Serves
    /// `[f, t]`.
    nfs: N,
    /// The seam watermark `w`: the FS's finalised tip height, or `None` when the
    /// FS holds nothing. Derived once at capture so the seam is fixed for the
    /// life of the pin.
    watermark: Option<Height>,
}

/// Which side of the seam a height falls on. The `InitialBuildGap` arm is the
/// explicit policy knob (see [`ChainViewSnapshot::read_routed`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    /// `h ≤ w`: the durable finalised prefix, read from the FS.
    Finalised,
    /// `f ≤ h ≤ t`: the volatile non-finalised window, read from the NFS.
    Volatile,
    /// `w < h < f`: on-chain, but held by neither side — the FS is still
    /// building up toward the NFS floor.
    InitialBuildGap,
    /// `h > t`, or no window at all: no such block.
    AboveTip,
}

impl<F, N> ChainViewSnapshot<F, N>
where
    F: ChainSegment + CompactBlockRead,
    N: ChainSegment + CompactBlockRead,
{
    /// Compose a pinned view from a finalised store segment and a non-finalised
    /// segment captured at the same instant. The watermark is the FS's coverage
    /// high read once here, so the seam is coherent for the life of the pin.
    pub(crate) fn new(fs: F, nfs: N) -> Self {
        // The FS covers `[genesis, w]`, so the high of its coverage *is* the
        // watermark `w` — `None` distinguishes an empty FS from a genesis-only
        // FS (`Some(0)`), which `serviceable_range().finalized_tip`
        // (GENESIS-on-empty) cannot.
        let watermark = fs.coverage().map(|range| range.end);
        Self { fs, nfs, watermark }
    }

    /// Classify `height` against the seam. Pure over the captured coordinates.
    fn route(&self, height: Height) -> Route {
        // FS covers [genesis, w].
        if let Some(watermark) = self.watermark
            && height <= watermark
        {
            return Route::Finalised;
        }
        // Above the watermark (or the FS is empty): consult the NFS window,
        // whose coverage names its floor and tip.
        match self.nfs.coverage() {
            Some(window) => {
                if height > window.end {
                    Route::AboveTip
                } else if height >= window.start {
                    Route::Volatile
                } else {
                    Route::InitialBuildGap
                }
            }
            // No servable window (an empty head): nothing above the watermark is
            // served.
            None => Route::AboveTip,
        }
    }

    /// Read the composed compact block at `height`, routing on the seam.
    ///
    /// The `InitialBuildGap` arm is a **policy knob**: this stage returns
    /// [`BlockReadError::NotServiceable`] for a height the FS has not yet built
    /// up to and the NFS window does not reach down to. It deliberately does
    /// **not** source-fill. A later stage's watermark handshake shrinks this gap
    /// to nothing *at the finalisation seam*; whether to source-fill the
    /// *initial-build* gap is a separate decision left open here.
    async fn read_routed(&self, height: Height) -> Result<Option<CompactBlock>, BlockReadError> {
        match self.route(height) {
            Route::Finalised => self.fs.compact_block(BlockRef::Height(height)).await,
            Route::Volatile => self.nfs.compact_block(BlockRef::Height(height)).await,
            Route::InitialBuildGap => Err(BlockReadError::NotServiceable(Capability::Blocks)),
            Route::AboveTip => Ok(None),
        }
    }
}

impl<F, N> ChainSegment for ChainViewSnapshot<F, N>
where
    F: ChainSegment + CompactBlockRead,
    N: ChainSegment + CompactBlockRead,
{
    fn pinned_tip(&self) -> Option<BlockId> {
        // The composed tip: the volatile NFS tip when present, else the
        // finalised tip the FS is pinned to.
        self.nfs.pinned_tip().or_else(|| self.fs.pinned_tip())
    }

    fn coverage(&self) -> Option<HeightRange> {
        // The composed span `FS ∪ NFS`: low is the FS floor (genesis) when the
        // FS holds anything, else the NFS floor; high is the NFS tip when the
        // head holds anything, else the FS high. `None` only when both sides are
        // empty.
        let fs = self.fs.coverage();
        let nfs = self.nfs.coverage();
        let start = fs.or(nfs).map(|range| range.start)?;
        let end = nfs.or(fs).map(|range| range.end)?;
        Some(HeightRange { start, end })
    }
}

impl<F, N> Snapshot for ChainViewSnapshot<F, N>
where
    F: ChainSegment + CompactBlockRead,
    N: ChainSegment + CompactBlockRead,
{
    fn serviceable_range(&self) -> ServiceableRange {
        let finalized_tip = self.watermark.unwrap_or(Height::GENESIS);
        // The served tip is the NFS tip height, falling back to the watermark
        // (or genesis) when the head is empty.
        let tip = self
            .nfs
            .pinned_tip()
            .map(|id| id.height)
            .unwrap_or(finalized_tip);
        ServiceableRange { finalized_tip, tip }
    }
}

impl<F, N> CompactBlockRead for ChainViewSnapshot<F, N>
where
    F: ChainSegment + CompactBlockRead,
    N: ChainSegment + CompactBlockRead,
{
    async fn compact_block(&self, at: BlockRef) -> Result<Option<CompactBlock>, BlockReadError> {
        match at {
            BlockRef::Height(height) => self.read_routed(height).await,
            // A hash is not seam-routable (no height until resolved), so resolve
            // it as `FS ∪ NFS`: the finalised store first, then the volatile
            // window.
            BlockRef::Hash(hash) => match self.fs.compact_block(BlockRef::Hash(hash)).await? {
                Some(block) => Ok(Some(block)),
                None => self.nfs.compact_block(BlockRef::Hash(hash)).await,
            },
        }
    }

    fn stream_compact(&self, range: HeightRange) -> BoxStream<'_, Result<CompactBlock, ReadError>> {
        // Eager per-height stitch across the seam, mirroring
        // `StoreSnapshot::stream_compact`: iterate the inclusive height span,
        // route each height, and yield in height order. A gap height surfaces as
        // an `Err` item (`NotServiceable`); a height above the tip is skipped
        // (it is `Ok(None)`, a domain absence, not a failure).
        let start = u32::from(range.start);
        let end = u32::from(range.end);
        let heights: Vec<Height> = (start..=end)
            .filter_map(|height| Height::try_from(height).ok())
            .collect();
        stream::iter(heights)
            .then(move |height| async move { self.read_routed(height).await })
            .filter_map(|routed| async move {
                match routed {
                    Ok(Some(block)) => Some(Ok(block)),
                    Ok(None) => None,
                    Err(error) => Some(Err(block_read_to_read_error(error))),
                }
            })
            .boxed()
    }
}

/// Map a [`BlockReadError`] onto the generic [`ReadError`] used by streamed
/// reads, preserving the not-serviceable / transient / fatal distinction.
fn block_read_to_read_error(error: BlockReadError) -> ReadError {
    match error {
        BlockReadError::NotServiceable(capability) => ReadError::NotServiceable(capability),
        BlockReadError::Transient(message) => ReadError::Transient(message),
        BlockReadError::Fatal(message) => ReadError::Fatal(message),
    }
}
