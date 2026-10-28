//! Error type for the mempool core.

use std::error::Error as StdError;

/// Errors surfaced by the mempool core and its ports.
///
/// A validator failure is type-erased into [`MempoolError::Source`] where it is
/// read. `zaino-source` gives each port its own `QueryError<E>`, and the mempool
/// treats every one of them the same way — keep the last set, mark it
/// [`IncompleteSourceError`](crate::MempoolCompleteness::IncompleteSourceError),
/// retry on the next poll — so carrying the distinction through this type would
/// be a generic parameter no consumer ever matches on.
///
/// The one validator answer the mempool *does* act on differently is
/// [`GetRawMempoolTransactionError::NotFound`](zaino_source::GetRawMempoolTransactionError::NotFound),
/// which means a transaction left the mempool between listing and fetch. That is
/// handled where it is read, by skipping the transaction, and never becomes a
/// `MempoolError` at all.
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
