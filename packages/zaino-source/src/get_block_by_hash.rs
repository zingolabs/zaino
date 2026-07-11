//! Query: fetch raw serialized block bytes by hash.

use std::future::Future;

use zaino_primitives::types::BlockHash;

use super::QueryError;

/// Domain error for [`GetBlockByHash`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetBlockByHashError {
    /// No block with this hash exists.
    #[error("block not found: {0}")]
    NotFound(BlockHash),
}

/// Fetch the raw serialized block identified by its hash.
///
/// Maps to `getblock(hash, 0)` over JSON-RPC, or the equivalent
/// ReadState query.
pub trait GetBlockByHash: Send + Sync {
    /// Fetch raw block bytes by hash.
    fn get_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> impl Future<Output = Result<Vec<u8>, QueryError<GetBlockByHashError>>> + Send;
}
