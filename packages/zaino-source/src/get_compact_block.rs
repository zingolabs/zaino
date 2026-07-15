//! Query: fetch a compact block at a given height.
//!
//! Compact blocks skip proofs, signatures, and input scripts —
//! only indexer-relevant fields are deserialized.

use std::future::Future;

use zaino_primitives::types::{CompactBlock, Height};

use super::QueryError;

// Reuse GetBlockError — same domain error (height not found).
pub use super::GetBlockError;

/// Fetch a compact block at a given height.
///
/// The adapter deserializes from its wire format into the domain
/// [`CompactBlock`] type, skipping proofs and signatures.
pub trait GetCompactBlock: Send + Sync {
    /// Fetch a compact block.
    fn get_compact_block(
        &self,
        height: Height,
    ) -> impl Future<Output = Result<CompactBlock, QueryError<GetBlockError>>> + Send;
}
