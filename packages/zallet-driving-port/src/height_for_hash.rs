//! Capability: look up the height of a block hash.

use std::future::Future;

use zaino_primitives::types::{BlockHash, Height};

use crate::error::PortError;

/// Domain error for [`GetHeightForHash`].
///
/// Empty: absence is an answer (`Ok(None)`), not a rejection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GetHeightForHashError {}

/// Look up the height of a block hash in the pinned view.
///
/// This is the membership probe fork-point detection builds on: a hash
/// answers `Some` exactly when its block is on the pinned best chain.
pub trait GetHeightForHash: Send + Sync {
    /// The height of `hash`, or `None` when the block is not in the
    /// pinned view.
    fn get_height_for_hash(
        &self,
        hash: BlockHash,
    ) -> impl Future<Output = Result<Option<Height>, PortError<GetHeightForHashError>>> + Send;
}
