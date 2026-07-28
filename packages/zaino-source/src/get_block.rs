//! Query: fetch a parsed block at a given height.

use std::future::Future;

use zaino_primitives::types::{Block, Height};

use super::QueryError;

/// Domain error for [`GetBlock`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetBlockError {
    /// No block exists at this height.
    #[error("no block at height {0}")]
    HeightNotFound(Height),
}

/// Fetch a fully parsed block at a given height.
///
/// The adapter deserializes from its wire format into the domain
/// [`Block`] type. The consumer receives typed data, not bytes.
pub trait GetBlock: Send + Sync {
    /// Fetch a parsed block.
    fn get_block(
        &self,
        height: Height,
    ) -> impl Future<Output = Result<Block, QueryError<GetBlockError>>> + Send;
}
