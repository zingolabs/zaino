//! The block subsidy split at a height, read live from the validator.
//!
//! A control rather than a snapshot read: the split is a consensus rule the
//! validator evaluates for any height, including one above the tip, so nothing
//! pins it to a chain view. Zaino carries no subsidy schedule of its own, so
//! this is always passthrough.

use std::future::Future;

use zaino_primitives::types::rpc::BlockSubsidy;
use zaino_primitives::types::Height;

use crate::error::ReadError;

/// Why a block-subsidy read could not be answered.
#[derive(Debug, thiserror::Error)]
pub enum BlockSubsidyReadError {
    /// The validator has no subsidy for this height.
    #[error("no block subsidy at height {0}")]
    HeightNotReached(Height),
    /// The read itself failed.
    #[error(transparent)]
    Read(#[from] ReadError),
}

/// `getblocksubsidy`: how the subsidy at a height divides between the miner,
/// the founders' reward, funding streams and lockboxes.
pub trait BlockSubsidyRead: Send + Sync {
    /// Fetch the subsidy split at `height`.
    fn block_subsidy(
        &self,
        height: Height,
    ) -> impl Future<Output = Result<BlockSubsidy, BlockSubsidyReadError>> + Send;
}
