//! Query: fetch commitment tree roots and sizes at a block.

use std::future::Future;

use zaino_primitives::types::{BlockHash, TreeRoots};

use super::QueryError;

/// Domain error for [`GetCommitmentTreeRoots`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetCommitmentTreeRootsError {
    /// No block with this hash exists.
    #[error("block not found: {0}")]
    BlockNotFound(BlockHash),
}

/// Fetch commitment tree roots and sizes at a specific block.
///
/// Available via Zebra ReadState; over JSON-RPC this is assembled
/// from `z_gettreestate`.
pub trait GetCommitmentTreeRoots: Send + Sync {
    /// Fetch tree roots.
    fn get_commitment_tree_roots(
        &self,
        block: BlockHash,
    ) -> impl Future<Output = Result<TreeRoots, QueryError<GetCommitmentTreeRootsError>>> + Send;
}
