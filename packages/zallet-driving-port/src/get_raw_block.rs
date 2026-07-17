//! Capability: read a block's consensus serialization by height.

use std::future::Future;

use zaino_primitives::types::Height;

use crate::error::PortError;
use crate::raw::RawBlock;

/// Domain error for [`GetRawBlock`].
///
/// Empty: absence is an answer (`Ok(None)`), not a rejection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GetRawBlockError {}

/// Read the consensus serialization of the block at a height in the
/// pinned view.
///
/// Height is unambiguous within a snapshot — the pinned chain has
/// exactly one block per height up to its tip.
pub trait GetRawBlock: Send + Sync {
    /// The block at `height`, or `None` when the height lies beyond
    /// the pinned tip.
    fn get_raw_block(
        &self,
        height: Height,
    ) -> impl Future<Output = Result<Option<RawBlock>, PortError<GetRawBlockError>>> + Send;
}
