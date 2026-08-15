//! Query: fetch note commitment subtree roots.

use std::future::Future;

use zaino_primitives::types::{ShieldedPool, SubtreeRoot};

use super::QueryError;

/// Domain error for [`GetSubtreeRoots`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetSubtreeRootsError {
    /// The requested pool is not supported or not active.
    #[error("pool unavailable: {0}")]
    PoolUnavailable(ShieldedPool),
}

/// Fetch note commitment subtree roots for a shielded pool.
///
/// Maps to `z_getsubtreesbyindex(pool, start_index, limit)` over
/// JSON-RPC.
pub trait GetSubtreeRoots: Send + Sync {
    /// Fetch subtree roots.
    fn get_subtree_roots(
        &self,
        pool: ShieldedPool,
        start_index: u16,
        limit: Option<u16>,
    ) -> impl Future<Output = Result<Vec<SubtreeRoot>, QueryError<GetSubtreeRootsError>>> + Send;
}
