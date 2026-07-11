//! Query: fetch commitment tree state at a given height.

use std::future::Future;

use zaino_primitives::types::Height;

use super::QueryError;

/// Serialized commitment tree bytes for one pool.
///
/// Opaque to this crate. Interpretation (Sapling vs Orchard,
/// deserialization into tree structures) happens in consumer crates.
pub type TreeBytes = Vec<u8>;

/// Commitment tree state at a block: Sapling and Orchard trees.
///
/// Either pool may be absent if the block predates that pool's activation.
#[derive(Debug, Clone)]
pub struct TreestateResponse {
    /// Serialized Sapling commitment tree, if active at this height.
    pub sapling: Option<TreeBytes>,
    /// Serialized Orchard commitment tree, if active at this height.
    pub orchard: Option<TreeBytes>,
}

/// Domain error for [`GetTreestate`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetTreestateError {
    /// No block exists at this height (can't compute treestate).
    #[error("no treestate at height {0}")]
    HeightNotFound(Height),
}

/// Fetch the commitment tree state at a given height.
///
/// Maps to `z_gettreestate(height)` over JSON-RPC, or the equivalent
/// ReadState query.
pub trait GetTreestate: Send + Sync {
    /// Fetch treestate.
    fn get_treestate(
        &self,
        height: Height,
    ) -> impl Future<Output = Result<TreestateResponse, QueryError<GetTreestateError>>> + Send;
}
