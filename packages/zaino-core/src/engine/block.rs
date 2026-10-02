//! Full block reads — always passthrough, except the tip.
//!
//! A full block can only come from the validator: the finalised store holds
//! compact projections, not full block bytes. The source port already returns
//! the domain [`Block`], so this routes a [`BlockSelector`] to the by-height or
//! by-hash port and reads the header off the block it already has — no separate
//! header or height port.
//!
//! The tip is the exception and stays local: it is what the snapshot is coherent
//! against. Asking the validator would return a tip the rest of the snapshot
//! does not share.
//!
//! Unlike the routed reads (`address`, `spend`, `treestate`), block placement
//! never varies, so there is no placement-trait dispatch — one impl, bounded on
//! the two source ports.

use futures::stream::{self, BoxStream, StreamExt};

use crate::chain_view::ChainTier;
use crate::routing::Routing;
use zaino_primitives::types::{
    Block, BlockHash, BlockHeader, BlockRef, BlockSelector, Height, HeightRange,
};
use zaino_service::error::{BlockReadError, ReadError};
use zaino_service::{BlockRead, Capability, ChainSegment};
use zaino_source::{GetBlock, GetBlockByHash};

use super::EngineSnapshot;

impl<F, N, Src, R> BlockRead for EngineSnapshot<F, N, Src, R>
where
    F: ChainTier,
    N: ChainTier,
    Src: GetBlock + GetBlockByHash + Send + Sync + 'static,
    R: Routing,
{
    async fn tip(&self) -> Result<BlockRef, BlockReadError> {
        // Local: the pinned tip is the coordinate the rest of the snapshot is
        // read against. An empty snapshot has none to serve yet.
        self.local()
            .pinned_tip()
            .ok_or(BlockReadError::NotServiceable(Capability::Blocks))
    }

    async fn block(&self, at: BlockSelector) -> Result<Option<Block>, BlockReadError> {
        match at {
            BlockSelector::Height(height) => self.passthrough().block(height).await,
            BlockSelector::Hash(hash) => self.passthrough().block_by_hash(hash).await,
        }
    }

    async fn block_header(&self, at: BlockSelector) -> Result<Option<BlockHeader>, BlockReadError> {
        // The header rides on the block the selector names; no separate port.
        Ok(self.block(at).await?.map(|block| block.header))
    }

    async fn block_height(&self, hash: BlockHash) -> Result<Option<Height>, BlockReadError> {
        // The height is a field of the named block's header.
        Ok(self
            .passthrough()
            .block_by_hash(hash)
            .await?
            .map(|block| block.header.height))
    }

    fn stream_blocks(&self, range: HeightRange) -> BoxStream<'_, Result<Block, ReadError>> {
        // Ascending over the inclusive `[start, end]` span, one block per height.
        // A height the validator does not hold is a domain absence and is
        // skipped; the first transport error is yielded and ends the stream, so a
        // consumer cannot mistake a truncated stream for a complete one. The
        // `Option` state carries that stop: it becomes `None` after an error, and
        // the next poll ends.
        let first = u32::from(range.start);
        let last = u32::from(range.end);
        let heights = (first..=last).filter_map(|height| Height::try_from(height).ok());
        stream::unfold(Some(heights), move |state| async move {
            let mut heights = state?;
            loop {
                let height = heights.next()?;
                match self.passthrough().block(height).await {
                    Ok(Some(block)) => return Some((Ok(block), Some(heights))),
                    Ok(None) => continue,
                    Err(error) => {
                        return Some((Err(block_read_to_read_error(error)), None));
                    }
                }
            }
        })
        .boxed()
    }
}

/// Map a [`BlockReadError`] onto the generic [`ReadError`] a streamed read
/// yields, preserving the not-serviceable / transient / fatal distinction.
fn block_read_to_read_error(error: BlockReadError) -> ReadError {
    match error {
        BlockReadError::NotServiceable(capability) => ReadError::NotServiceable(capability),
        BlockReadError::Transient(message) => ReadError::Transient(message),
        BlockReadError::Fatal(message) => ReadError::Fatal(message),
    }
}
