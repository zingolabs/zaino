//! Query: fetch raw serialized block bytes at a given height.

use core::fmt;
use std::future::Future;

use zaino_primitives::types::Height;

use super::TransportError;

/// Domain error for [`GetBlockBytes`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GetBlockBytesError {
    /// No block exists at this height.
    HeightNotFound(Height),
}

impl fmt::Display for GetBlockBytesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HeightNotFound(h) => write!(f, "no block at height {h}"),
        }
    }
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
    ) -> impl Future<Output = Result<Vec<u8>, QueryError>> + Send;
}

/// Combined domain + transport error for this query.
#[derive(Debug)]
pub enum QueryError {
    /// The question has a valid answer: "no such block."
    Domain(GetBlockBytesError),
    /// The question couldn't be delivered or the response couldn't be parsed.
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

impl From<GetBlockBytesError> for QueryError {
    fn from(e: GetBlockBytesError) -> Self {
        Self::Domain(e)
    }
}

impl From<TransportError> for QueryError {
    fn from(e: TransportError) -> Self {
        Self::Transport(e)
    }
}
