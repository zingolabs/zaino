//! Query: fetch per-transaction transparent value movements for a block.

use std::future::Future;

use zaino_primitives::types::{rpc::BlockDeltas, BlockHash};

use super::QueryError;

/// Domain error for [`GetBlockDeltas`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetBlockDeltasError {
    /// No block with this hash is known.
    #[error("no block with hash {0}")]
    BlockNotFound(BlockHash),
}

/// Fetch the transparent value movements of every transaction in a block.
///
/// The result is deliberately incomplete — see
/// [`BlockDelta`](zaino_primitives::types::rpc::BlockDelta) — because the
/// validator omits any input or output it cannot attribute to exactly one
/// transparent address.
///
/// Maps to `getblockdeltas` over JSON-RPC.
pub trait GetBlockDeltas: Send + Sync {
    /// Fetch a block's transparent deltas.
    fn get_block_deltas(
        &self,
        hash: BlockHash,
    ) -> impl Future<Output = Result<BlockDeltas, QueryError<GetBlockDeltasError>>> + Send;
}
