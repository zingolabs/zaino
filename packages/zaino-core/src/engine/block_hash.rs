//! The `getblockhashes` timestamp-range read — always local.
//!
//! Zcash block timestamps are not monotonic, so a timestamp range is not a slice
//! of the height axis. This read drives the pure [`CandidateSearch`] over the
//! composed chain view's [`header`](zaino_service::HeaderRead::header) reads: the
//! search derives the smallest height bracket guaranteed to contain every block
//! with `low <= nTime < high` (from the median-time-past consensus rule), then
//! this read scans that bracket and keeps the blocks whose own timestamp is in
//! range. The filter is exact because the bracket is a guaranteed superset.
//!
//! Local over the chain view, like [`CompactBlockRead`](zaino_service::CompactBlockRead):
//! it reads the pinned snapshot's two tiers through `header`, routed on the seam,
//! and needs no source port. Its placement never varies, so there is no
//! placement-trait dispatch — one impl, over any routing.

use crate::chain_view::ChainTier;
use crate::routing::Routing;
use zaino_consensus::block_time::{CandidateSearch, MaxBlockTimeDrift, SearchStep};
use zaino_primitives::types::{BlockHash, BlockTime, Height};
use zaino_service::error::BlockHashReadError;
use zaino_service::{BlockHashAt, BlockHashRead, ChainSegment, HeaderRead};

use super::EngineSnapshot;

impl<F, N, Src, R> BlockHashRead for EngineSnapshot<F, N, Src, R>
where
    F: ChainTier,
    N: ChainTier,
    Src: Clone + Send + Sync + 'static,
    R: Routing,
{
    async fn block_hashes(
        &self,
        low: BlockTime,
        high: BlockTime,
    ) -> Result<Vec<BlockHashAt>, BlockHashReadError> {
        let view = self.local();

        // Clamp to the pinned tip: the search reasons only over blocks the
        // snapshot is coherent against, and an empty view holds none. A range
        // beyond the tip is an empty list, never an error.
        let Some(tip) = view.pinned_tip().map(|id| id.height) else {
            return Ok(Vec::new());
        };

        // Drive the pure candidate-bracket search over the chain view's header
        // reads. The search requests only heights at or below the tip, where a
        // block must exist, so a `None` header is a chain-view hole it fails loud
        // on rather than narrowing the bracket and dropping an in-range block.
        //
        // The drift rule is the **mainnet** one (`nTime <= MTP + 90 min`,
        // enforced at every height): the node-RPC deployment that serves the
        // explorer's `getblockhashes` runs on mainnet. A testnet deployment would
        // need `MaxBlockTimeDrift::TESTNET` here so the pre-activation region
        // falls back to a full scan; the engine carries no network today, so this
        // is the documented assumption, not a derived choice.
        let mut search = CandidateSearch::new(tip, low, high, MaxBlockTimeDrift::MAINNET);
        let bracket = loop {
            match search.poll() {
                SearchStep::Need(height) => {
                    let time = view.header(height).await?.map(|summary| summary.time);
                    search.supply(height, time);
                }
                SearchStep::Done(bracket) => break bracket,
                SearchStep::Failed(missing) => {
                    return Err(BlockHashReadError::MissingHeader {
                        height: missing.height,
                    });
                }
            }
        };

        let Some(bracket) = bracket else {
            return Ok(Vec::new());
        };

        // Scan the bracket — a guaranteed superset within `[genesis, tip]` — and
        // keep the blocks whose own timestamp lies in the half-open range. A
        // `None` header here is a hole below the tip (above-tip heights are
        // excluded by the clamp), surfaced loud rather than dropping a block.
        let mut hits: Vec<BlockHashAt> = Vec::new();
        for height in heights(bracket.start, bracket.end) {
            let summary = view
                .header(height)
                .await?
                .ok_or(BlockHashReadError::MissingHeader { height })?;
            if low <= summary.time && summary.time < high {
                hits.push(BlockHashAt {
                    height,
                    hash: summary.hash,
                    time: summary.time,
                });
            }
        }

        // zcashd's timestamp index yields hashes ascending by timestamp, then by
        // hash; mirror that order. The explorer reverses the list itself.
        hits.sort_by(|a, b| a.time.cmp(&b.time).then_with(|| a.hash.cmp(&b.hash)));
        Ok(hits)
    }

    async fn block_hash(&self, height: Height) -> Result<Option<BlockHash>, BlockHashReadError> {
        let view = self.local();

        // Local over the chain view's header read, mirroring `block_hashes`'
        // above-tip vs hole-below-tip split. An empty view holds no block at any
        // height, and a height above the pinned tip is the domain miss `Ok(None)`
        // — never a validator block fetch.
        let Some(tip) = view.pinned_tip().map(|id| id.height) else {
            return Ok(None);
        };
        if height > tip {
            return Ok(None);
        }

        // At or below the tip a block must exist, so a `None` header is a
        // chain-view hole, surfaced loud rather than collapsed into a miss.
        let summary = view
            .header(height)
            .await?
            .ok_or(BlockHashReadError::MissingHeader { height })?;
        Ok(Some(summary.hash))
    }
}

/// The inclusive height span `[start, end]`, as valid heights.
fn heights(start: Height, end: Height) -> impl Iterator<Item = Height> {
    (u32::from(start)..=u32::from(end)).filter_map(|height| Height::try_from(height).ok())
}
