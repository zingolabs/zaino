//! Query: fetch the current network difficulty.

use std::future::Future;

use zaino_primitives::types::Difficulty;

use super::QueryError;

/// Domain error for [`GetDifficulty`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetDifficultyError {
    /// The validator has no chain tip to measure difficulty against.
    #[error("validator not ready")]
    NotReady,
}

/// Fetch the current difficulty, as a multiple of the network minimum.
///
/// Maps to `getdifficulty` over JSON-RPC.
pub trait GetDifficulty: Send + Sync {
    /// Fetch current difficulty.
    fn get_difficulty(
        &self,
    ) -> impl Future<Output = Result<Difficulty, QueryError<GetDifficultyError>>> + Send;
}
