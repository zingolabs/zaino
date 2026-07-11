//! Query: fetch raw serialized block bytes at a given height.

use std::future::Future;

use zaino_primitives::types::Height;

use super::QueryError;

/// Domain error for [`GetBlockBytes`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetBlockBytesError {
    /// No block exists at this height.
    #[error("no block at height {0}")]
    HeightNotFound(Height),
}

/// Fetch the raw serialized block at a given height.
///
/// Maps to `getblock(height, 0)` over JSON-RPC, or the equivalent
/// ReadState query.
pub trait GetBlockBytes: Send + Sync {
    /// Fetch raw block bytes.
    fn get_block_bytes(
        &self,
        height: Height,
    ) -> impl Future<Output = Result<Vec<u8>, QueryError<GetBlockBytesError>>> + Send;
}
