//! Query: fetch commitment tree state at a given block hash.

use std::future::Future;

use zaino_primitives::types::{BlockHash, Treestate};

use super::QueryError;

/// Domain error for [`GetTreestateByHash`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetTreestateByHashError {
    /// No block with this hash is known.
    #[error("no block with hash {0}")]
    BlockNotFound(BlockHash),
}

/// Fetch the commitment tree state after a block, addressed by hash.
///
/// The hash-addressed counterpart of [`GetTreestate`](super::GetTreestate),
/// which takes a height. Separate traits rather than one hash-or-height
/// argument, matching [`GetBlock`](super::GetBlock) and
/// [`GetBlockByHash`](super::GetBlockByHash): a height names a best-chain
/// block, whereas a hash can name a block on a side chain, so the two are
/// different questions that adapters may answer from different places.
pub trait GetTreestateByHash: Send + Sync {
    /// Fetch treestate at a block hash.
    fn get_treestate_by_hash(
        &self,
        hash: BlockHash,
    ) -> impl Future<Output = Result<Treestate, QueryError<GetTreestateByHashError>>> + Send;
}
