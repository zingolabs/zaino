//! Query: fetch commitment tree state at a given height.

use std::future::Future;

use zaino_primitives::types::{Height, Treestate};

use super::QueryError;

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
    ) -> impl Future<Output = Result<Treestate, QueryError<GetTreestateError>>> + Send;
}
