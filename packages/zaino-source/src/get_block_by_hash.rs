//! Query: fetch a parsed block by hash.

use std::future::Future;

use zaino_primitives::types::{Block, BlockHash};

use super::QueryError;

/// Domain error for [`GetBlockByHash`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetBlockByHashError {
    /// No block with this hash exists.
    #[error("block not found: {0}")]
    NotFound(BlockHash),
}

/// Fetch a fully parsed block identified by its hash.
pub trait GetBlockByHash: Send + Sync {
    /// Fetch a parsed block by hash.
    fn get_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> impl Future<Output = Result<Block, QueryError<GetBlockByHashError>>> + Send;
}
