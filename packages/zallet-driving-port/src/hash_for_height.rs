//! Capability: look up the block hash at a height.

use std::future::Future;

use zaino_primitives::types::{BlockHash, Height};

use crate::error::PortError;

/// Domain error for [`GetHashForHeight`].
///
/// Empty: absence is an answer (`Ok(None)`), not a rejection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GetHashForHeightError {}

/// Look up the hash of the block at a height in the pinned view.
pub trait GetHashForHeight: Send + Sync {
    /// The hash at `height`, or `None` when the height lies beyond the
    /// pinned tip.
    fn get_hash_for_height(
        &self,
        height: Height,
    ) -> impl Future<Output = Result<Option<BlockHash>, PortError<GetHashForHeightError>>> + Send;
}
