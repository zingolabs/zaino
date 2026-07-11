//! Query: fetch commitment tree state at a given height.

use core::fmt;
use std::future::Future;

use zaino_primitives::types::Height;

use super::TransportError;

/// Serialized commitment tree bytes for one pool.
///
/// Opaque to the primitives crate. Interpretation (Sapling vs Orchard,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GetTreestateError {
    /// No block exists at this height (can't compute treestate).
    HeightNotFound(Height),
}

impl fmt::Display for GetTreestateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HeightNotFound(h) => write!(f, "no treestate at height {h}"),
        }
    }
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
    ) -> impl Future<Output = Result<TreestateResponse, QueryError>> + Send;
}

/// Combined domain + transport error for this query.
#[derive(Debug)]
pub enum QueryError {
    /// Domain-level failure.
    Domain(GetTreestateError),
    /// Transport-level failure.
    Transport(TransportError),
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Domain(e) => write!(f, "{e}"),
            Self::Transport(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for QueryError {}

impl From<GetTreestateError> for QueryError {
    fn from(e: GetTreestateError) -> Self {
        Self::Domain(e)
    }
}

impl From<TransportError> for QueryError {
    fn from(e: TransportError) -> Self {
        Self::Transport(e)
    }
}
