//! Query: fetch a block header with the validator's derived chain state.

use std::future::Future;

use zaino_primitives::types::{rpc::BlockHeaderVerbose, BlockHash};

use super::QueryError;

/// Domain error for [`GetBlockHeader`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetBlockHeaderError {
    /// No block with this hash is known.
    #[error("no block with hash {0}")]
    BlockNotFound(BlockHash),
}

/// Fetch a block header together with the values the validator derives from
/// cumulative chain state — confirmations, difficulty, chainwork, and the
/// neighbouring block hashes.
///
/// The raw serialised header is a separate query
/// ([`GetRawBlockHeader`](super::GetRawBlockHeader)) rather than a verbosity
/// flag on this one: the caller already knows which form it wants, so the
/// choice belongs in the request, not in a response the caller must match on.
///
/// Maps to `getblockheader(hash, verbose = true)` over JSON-RPC.
pub trait GetBlockHeader: Send + Sync {
    /// Fetch a verbose block header.
    fn get_block_header(
        &self,
        hash: BlockHash,
    ) -> impl Future<Output = Result<BlockHeaderVerbose, QueryError<GetBlockHeaderError>>> + Send;
}
