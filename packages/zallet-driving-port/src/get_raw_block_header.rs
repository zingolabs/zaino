//! Capability: read a block header's consensus serialization by height.

use std::future::Future;

use zaino_primitives::types::Height;

use crate::error::PortError;
use crate::raw::RawBlockHeader;

/// Domain error for [`GetRawBlockHeader`].
///
/// Empty: absence is an answer (`Ok(None)`), not a rejection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GetRawBlockHeaderError {}

/// Read the consensus serialization of the header of the block at a
/// height in the pinned view.
///
/// Lighter than [`crate::GetRawBlock`] when only the header is needed.
/// Consensus serialization makes the header the prefix of the block's
/// own serialization, and the conformance kit holds implementations to
/// that.
pub trait GetRawBlockHeader: Send + Sync {
    /// The header at `height`, or `None` when the height lies beyond
    /// the pinned tip.
    fn get_raw_block_header(
        &self,
        height: Height,
    ) -> impl Future<Output = Result<Option<RawBlockHeader>, PortError<GetRawBlockHeaderError>>> + Send;
}
