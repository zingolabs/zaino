//! Capability: stream consensus-serialized blocks over a height range.

use std::ops::Range;

use futures_core::Stream;
use zaino_primitives::types::Height;

use crate::block_id::BlockId;
use crate::error::PortError;
use crate::raw::RawBlock;

/// Domain error for [`StreamRawBlocks`].
///
/// Empty: a range with nothing under it is an answer (an empty
/// stream), not a rejection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum StreamRawBlocksError {}

/// Stream the pinned view's blocks over a half-open height range.
///
/// The stream yields exactly the blocks of `range` that lie in the
/// pinned view, in ascending height order, each alongside its
/// [`BlockId`]. A range reaching beyond the pinned tip is clamped to
/// it; an empty or inverted range yields nothing. Streaming to the tip
/// is a composition: `start..tip.height + 1` with the tip from
/// [`crate::GetPinnedTip`].
pub trait StreamRawBlocks: Send + Sync {
    /// The blocks of `range` in the pinned view, ascending by height.
    fn stream_raw_blocks(
        &self,
        range: Range<Height>,
    ) -> impl Stream<Item = Result<(BlockId, RawBlock), PortError<StreamRawBlocksError>>> + Send;
}
