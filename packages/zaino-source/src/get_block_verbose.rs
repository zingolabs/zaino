//! Query: fetch verbose block metadata at a given height.

use std::future::Future;

use zaino_primitives::types::{BlockHash, BlockVerbose, Height};

use super::QueryError;

/// Domain error for [`GetBlockVerbose`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetBlockVerboseError {
    /// No block exists at this height.
    #[error("no block at height {0}")]
    HeightNotFound(Height),

    /// No block with this hash exists.
    ///
    /// Distinct from [`HeightNotFound`](Self::HeightNotFound) because
    /// [`GetBlockVerboseByHash`] shares this error type and has no height to
    /// report — naming the block by a height it was never asked about would
    /// misreport which lookup failed.
    #[error("no block with hash {0}")]
    BlockNotFound(BlockHash),
}

/// Fetch verbose block metadata at a given height.
///
/// Maps to `getblock(height, 1)` over JSON-RPC.
pub trait GetBlockVerbose: Send + Sync {
    /// Fetch verbose metadata.
    fn get_block_verbose(
        &self,
        height: Height,
    ) -> impl Future<Output = Result<BlockVerbose, QueryError<GetBlockVerboseError>>> + Send;
}

/// Fetch a block's chain-state facts, addressed by hash.
///
/// Separate from [`GetBlockVerbose`] for the same reason as
/// [`GetBlockByHash`](super::GetBlockByHash): a height names a best-chain
/// block, whereas a hash can name one on a side chain — where `confirmations`
/// is negative and there is no next block.
pub trait GetBlockVerboseByHash: Send + Sync {
    /// Fetch verbose metadata by block hash.
    fn get_block_verbose_by_hash(
        &self,
        hash: zaino_primitives::types::BlockHash,
    ) -> impl Future<Output = Result<BlockVerbose, QueryError<GetBlockVerboseError>>> + Send;
}
