//! Error types for the block store.

use thiserror::Error;

/// Errors from block store operations.
#[derive(Debug, Error)]
pub enum StoreError {
    /// Height not found in the height deque.
    #[error("height not found in deque: {0}")]
    HeightNotFound(u32),

    /// Height below freeze horizon — query LMDB instead.
    #[error("height {0} is below freeze horizon {1}")]
    BelowFreezeHorizon(u32, u32),

    /// Insertion precondition violated (e.g., hash already exists).
    #[error("insertion failed: {0}")]
    InsertionFailed(String),

    /// Freeze / LMDB error.
    #[error("freeze error: {0}")]
    FreezeError(String),

    /// Internal invariant violation.
    #[error("invariant violation: {0}")]
    InvariantViolation(String),
}

/// Errors from the sync loop.
#[derive(Debug, Error)]
pub enum SyncError {
    /// The fetcher returned an error.
    #[error("fetch error: {0}")]
    Fetch(String),

    /// The store returned an error.
    #[error("store error: {0}")]
    Store(#[from] StoreError),

    /// The remote chain changed during a fork-point search (reorg in
    /// flight). The fetched block at the expected height does not match
    /// the hash we were following, so the backward walk is incoherent.
    /// The caller should discard this attempt and retry.
    #[error(
        "chain incoherent during fork search: expected hash {expected:?} at height \
         {height}, got {got:?}"
    )]
    ChainIncoherent {
        /// Height at which the mismatch was detected.
        height: u32,
        /// Hash the backward walk expected at this height.
        expected: [u8; 32],
        /// Hash the validator actually returned.
        got: [u8; 32],
    },

    /// The fork point was not found within the maximum reorg depth.
    /// The remote chain diverged further back than the fuel allowed —
    /// the reorg is too deep to resolve via short sync.
    #[error("reorg too deep: fork not found within {depth} blocks")]
    ReorgTooDeep {
        /// Maximum search depth that was exhausted.
        depth: u32,
    },
}
