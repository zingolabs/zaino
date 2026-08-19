//! Query: fetch the block subsidy split at a given height.

use std::future::Future;

use zaino_primitives::types::{rpc::BlockSubsidy, Height};

use super::QueryError;

/// Domain error for [`GetBlockSubsidy`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetBlockSubsidyError {
    /// The height is above the chain tip, so its subsidy is not yet determined.
    #[error("height {0} is above the chain tip")]
    HeightNotReached(Height),
}

/// Fetch how the block subsidy at a height divides between the miner, the
/// founders' reward, funding streams and development lockboxes.
///
/// Maps to `getblocksubsidy` over JSON-RPC.
pub trait GetBlockSubsidy: Send + Sync {
    /// Fetch the subsidy split at a height.
    fn get_block_subsidy(
        &self,
        height: Height,
    ) -> impl Future<Output = Result<BlockSubsidy, QueryError<GetBlockSubsidyError>>> + Send;
}
