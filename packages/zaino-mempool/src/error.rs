//! Error type for the mempool core.

use std::error::Error as StdError;

/// Errors surfaced by the mempool core and its ports.
///
/// Source-layer failures are type-erased into [`MempoolError::Source`] at the
/// adapter boundary so the core does not name the concrete backend error type
/// (which lives in `zaino-state`).
#[derive(Debug, thiserror::Error)]
pub enum MempoolError {
    /// The backing mempool source (adapter) returned an error.
    #[error("mempool source error: {0}")]
    Source(Box<dyn StdError + Send + Sync>),

    /// The caller's chain tip does not match the tip the mempool snapshot is
    /// valid for. Retryable: the caller should re-snapshot and try again.
    #[error("mempool snapshot does not match the requested chain tip")]
    IncorrectChainTip,

    /// The mempool service is shutting down.
    #[error("mempool service is closing")]
    Closing,
}

impl MempoolError {
    /// Wrap an arbitrary adapter/source error as a [`MempoolError::Source`].
    pub fn source<E>(error: E) -> Self
    where
        E: StdError + Send + Sync + 'static,
    {
        MempoolError::Source(Box::new(error))
    }
}
