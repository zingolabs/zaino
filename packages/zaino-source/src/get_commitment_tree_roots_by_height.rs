//! Query: fetch commitment tree roots and sizes at a best-chain height.

use std::future::Future;

use zaino_primitives::types::{BlockHash, Height, TreeRoots};

use super::QueryError;

/// Domain error for [`GetCommitmentTreeRootsByHeight`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetCommitmentTreeRootsByHeightError {
    /// No best-chain block exists at this height.
    #[error("no best-chain block at height {0}")]
    HeightNotFound(Height),
}

/// Fetch commitment tree roots and sizes at the best-chain block at a height.
///
/// The answer names the block it describes: the best chain can reorganise
/// between a height-addressed read and a hash-addressed one, so a consumer
/// pairing this query with another must compare the returned hash against the
/// block it holds.
pub trait GetCommitmentTreeRootsByHeight: Send + Sync {
    /// Fetch tree roots at the best-chain block at this height, reporting which block answered.
    fn get_commitment_tree_roots_by_height(
        &self,
        height: Height,
    ) -> impl Future<
        Output = Result<(BlockHash, TreeRoots), QueryError<GetCommitmentTreeRootsByHeightError>>,
    > + Send;
}
