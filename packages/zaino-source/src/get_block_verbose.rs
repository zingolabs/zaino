//! Query: fetch verbose block metadata at a given height.

use std::future::Future;

use zaino_primitives::types::{BlockVerbose, Height};

use super::QueryError;

/// Domain error for [`GetBlockVerbose`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetBlockVerboseError {
    /// No block exists at this height.
    #[error("no block at height {0}")]
    HeightNotFound(Height),
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
