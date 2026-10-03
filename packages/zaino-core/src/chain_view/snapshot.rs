//! The composed, pinned view served across the FS⊕NFS seam.

use crate::chain_view::ChainTier;
use futures::stream::{self, BoxStream, StreamExt};

use zaino_primitives::types::{BlockRef, BlockSelector, CompactBlock, Height, HeightRange};
use zaino_service::error::{BlockReadError, ReadError};
use zaino_service::{Capability, ServiceableRange};
use zaino_service::{ChainSegment, CompactBlockRead, HeaderRead, HeaderSummary, Snapshot};

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
    F: ChainTier,
    N: ChainTier,
{
    /// Compose a pinned view from a finalised store segment and a non-finalised
    /// segment captured at the same instant. The watermark is the FS's coverage
    /// high read once here, so the seam is coherent for the life of the pin.
    pub(crate) fn new(fs: F, nfs: N) -> Self {
        // The FS covers `[genesis, w]`, so the high of its coverage *is* the
        // watermark `w` — `None` distinguishes an empty FS from a genesis-only
        // FS (`Some(0)`).
        let watermark = fs.coverage().map(|range| range.end);
        Self { fs, nfs, watermark }
    }

    /// The finalised segment, as pinned. A composer merging a read across the
    /// seam asks each side for its half; the split point is
    /// [`watermark`](Self::watermark).
    pub fn finalised(&self) -> &F {
        &self.fs
    }

    /// The non-finalised segment, as pinned. See [`finalised`](Self::finalised).
    pub fn non_finalised(&self) -> &N {
        &self.nfs
    }

    /// The seam watermark `w` captured with this pin: the finalised store's
    /// tip, or `None` when it holds nothing. Heights `≤ w` are the finalised
    /// side's; above it, the volatile window's.
    pub fn watermark(&self) -> Option<Height> {
        self.watermark
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
            Route::Finalised => self.fs.compact_block(BlockSelector::Height(height)).await,
            Route::Volatile => self.nfs.compact_block(BlockSelector::Height(height)).await,
            Route::InitialBuildGap => Err(BlockReadError::NotServiceable(Capability::Blocks)),
            Route::AboveTip => Ok(None),
        }
    }

    /// Read the header projection at `height`, routing on the seam with the
    /// **same** [`route`](Self::route) the compact-block read uses, so a header
    /// and a compact block at one height are always served by the same tier. The
    /// `InitialBuildGap` arm is the same policy knob as
    /// [`read_routed`](Self::read_routed).
    async fn read_routed_header(
        &self,
        height: Height,
    ) -> Result<Option<HeaderSummary>, BlockReadError> {
        match self.route(height) {
            Route::Finalised => self.fs.header(height).await,
            Route::Volatile => self.nfs.header(height).await,
            Route::InitialBuildGap => Err(BlockReadError::NotServiceable(Capability::Blocks)),
            Route::AboveTip => Ok(None),
        }
    }
}

impl<F, N> ChainSegment for ChainViewSnapshot<F, N>
where
    F: ChainTier,
    N: ChainTier,
{
    fn pinned_tip(&self) -> Option<BlockRef> {
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
    F: ChainTier,
    N: ChainTier,
{
    fn serviceable_range(&self) -> Option<ServiceableRange> {
        // The served tip is the head's tip, falling back to the watermark when
        // the head is empty; with neither, the view holds nothing.
        let tip = self
            .nfs
            .pinned_tip()
            .map(|id| id.height)
            .or(self.watermark)?;
        Some(ServiceableRange {
            watermark: self.watermark,
            tip,
        })
    }
}

impl<F, N> CompactBlockRead for ChainViewSnapshot<F, N>
where
    F: ChainTier,
    N: ChainTier,
{
    async fn compact_block(
        &self,
        at: BlockSelector,
    ) -> Result<Option<CompactBlock>, BlockReadError> {
        match at {
            BlockSelector::Height(height) => self.read_routed(height).await,
            // A hash is not seam-routable (no height until resolved), so resolve
            // it as `FS ∪ NFS`: the finalised store first, then the volatile
            // window.
            BlockSelector::Hash(hash) => {
                match self.fs.compact_block(BlockSelector::Hash(hash)).await? {
                    Some(block) => Ok(Some(block)),
                    None => self.nfs.compact_block(BlockSelector::Hash(hash)).await,
                }
            }
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
                    Err(error) => Some(Err(error.into())),
                }
            })
            .boxed()
    }
}

impl<F, N> HeaderRead for ChainViewSnapshot<F, N>
where
    F: ChainTier,
    N: ChainTier,
{
    async fn header(&self, h: Height) -> Result<Option<HeaderSummary>, BlockReadError> {
        self.read_routed_header(h).await
    }
}

#[cfg(test)]
mod tests {
    use super::ChainViewSnapshot;
    use crate::testing::{StubNonFinalised, stub_compact_block};
    use zaino_primitives::types::{CompactBlock, Height};
    use zaino_service::HeaderRead;

    /// A stub block at `height` whose hash byte is `hash_byte` and whose time is
    /// `time`, so a header read carries values that identify the tier it came
    /// from.
    fn block_at(height: u32, hash_byte: u8, time: u32) -> CompactBlock {
        let mut block = stub_compact_block(height, hash_byte);
        block.time = time;
        block
    }

    fn height(h: u32) -> Height {
        Height::try_from(h).expect("valid test height")
    }

    /// FS covers `[0, 5]` (hash byte `0xAA`, times `1000 + h`); NFS covers
    /// `[3, 10]` (hash byte `0xBB`, times `2000 + h`). The overlap `[3, 5]` is on
    /// both tiers with **different** hashes and times, so a mis-route in the
    /// finalised band returns the NFS values and fails the assertion. Watermark
    /// = the FS coverage high = 5.
    fn composed() -> ChainViewSnapshot<StubNonFinalised, StubNonFinalised> {
        let fs =
            StubNonFinalised::from_blocks((0..=5).map(|h| block_at(h, 0xAA, 1000 + h)).collect());
        let nfs =
            StubNonFinalised::from_blocks((3..=10).map(|h| block_at(h, 0xBB, 2000 + h)).collect());
        ChainViewSnapshot::new(fs, nfs)
    }

    #[tokio::test]
    async fn header_below_the_watermark_reads_the_finalised_tier() {
        let view = composed();
        let summary = view
            .header(height(4))
            .await
            .expect("read succeeds")
            .expect("height 4 is covered");
        // The finalised tier owns `[0, 5]` even where the NFS window overlaps it.
        assert_eq!(<[u8; 32]>::from(summary.hash)[0], 0xAA);
        assert_eq!(summary.time, 1004);
    }

    #[tokio::test]
    async fn header_above_the_watermark_reads_the_non_finalised_tier() {
        let view = composed();
        let summary = view
            .header(height(8))
            .await
            .expect("read succeeds")
            .expect("height 8 is covered");
        assert_eq!(<[u8; 32]>::from(summary.hash)[0], 0xBB);
        assert_eq!(summary.time, 2008);
    }

    #[tokio::test]
    async fn header_above_the_tip_is_a_domain_miss() {
        let view = composed();
        assert_eq!(view.header(height(20)).await.expect("read succeeds"), None);
    }
}
