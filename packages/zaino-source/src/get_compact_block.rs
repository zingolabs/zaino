//! Query: fetch a pre-index compact block at a given height.
//!
//! The pre-index compact block has all per-block data needed for indexing
//! (proofs/sigs stripped) but no indexed state like commitment tree sizes.

use std::future::Future;

use zaino_primitives::types::{Height, PreIndexCompactBlock};

use super::QueryError;

pub use super::GetBlockError;

/// Fetch a pre-index compact block at a given height.
///
/// The adapter deserializes from its wire format into
/// [`PreIndexCompactBlock`], skipping proofs and signatures.
pub trait GetPreIndexCompactBlock: Send + Sync {
    /// Fetch a pre-index compact block.
    fn get_pre_index_compact_block(
        &self,
        height: Height,
    ) -> impl Future<Output = Result<PreIndexCompactBlock, QueryError<GetBlockError>>> + Send;
}
