//! What can go wrong inside this backend.
//!
//! # Temporary
//!
//! This is the finalised state's error enum, moved and renamed. It is not
//! [`zaino_chain_store::ChainStoreError`], which is what the ports return and
//! what a consumer should see; mapping onto that happens where the ports are
//! implemented. Keeping the internal enum lets the implementation move without
//! rewriting every `?` in it at the same time.

use zaino_chain_store::ChainStoreSourceError;

use crate::types::BlockHash;

/// Something went wrong inside the store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The validator could not answer something the store needed.
    #[error("backing block source failed: {0}")]
    Source(#[from] ChainStoreSourceError),

    /// Custom Errors.
    // TODO: Remove before production
    #[error("Custom error: {0}")]
    Custom(String),

    /// Requested data is missing from the finalised state.
    ///
    /// This could be due to the database not yet being synced or due to a bad
    /// request input.
    #[error("Missing data: {0}")]
    DataUnavailable(String),

    /// A block is present on disk but failed internal validation.
    ///
    /// *Typically means: checksum mismatch, corrupt CBOR, Merkle check
    /// failed, etc.*  The caller should fetch the correct data and
    /// overwrite the faulty block.
    #[error("invalid block @ height {height} (hash {hash}): {reason}")]
    InvalidBlock {
        /// The height the bad block was read from.
        height: u32,
        /// The hash the bad block claims.
        hash: BlockHash,
        /// What failed to validate.
        reason: String,
    },

    /// Returned when a caller asks for a feature that the
    /// currently-opened database version does not advertise.
    #[error("feature unavailable: {0}")]
    FeatureUnavailable(&'static str),

    /// Critical Errors, Restart Zaino.
    #[error("Critical error: {0}")]
    Critical(String),

    /// Error from the LMDB database.
    #[error("LMDB database error: {0}")]
    LmdbError(#[from] lmdb::Error),

    /// Serde Json serialisation / deserialisation errors.
    #[error("JSON error: {0}")]
    SerdeJsonError(#[from] serde_json::Error),

    /// Unexpected status-related error.
    #[error("Status error: {0:?}")]
    StatusError(StatusError),

    /// std::io::Error
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
}

/// A general error type to represent error StatusTypes.
#[derive(Debug, Clone, thiserror::Error)]
#[error("Unexpected status error: {server_status:?}")]
pub struct StatusError {
    /// The status the store was in when the error was raised.
    pub server_status: zaino_status::StatusType,
}

/// A validator's answer to one question, as a source error.
///
/// The `zaino-source` ports carry a per-question error type, so there is no one
/// `From` impl that covers them; this collapses any of them onto the domain's
/// three-way classification. The distinction that matters to the store is
/// whether retrying can help, so a domain-level rejection — the validator
/// answering "no such block" — becomes [`ChainStoreSourceError::NotReady`]
/// rather than a transport failure: the store only asks for heights it believes
/// exist, so a rejection means the validator has not caught up.
pub(crate) fn source_error<E: core::fmt::Debug + core::fmt::Display>(
    error: zaino_source::QueryError<E>,
) -> ChainStoreSourceError {
    match error {
        zaino_source::QueryError::Domain(error) => {
            ChainStoreSourceError::not_ready(error.to_string())
        }
        zaino_source::QueryError::Fetch(error) => {
            ChainStoreSourceError::unavailable(error.to_string())
        }
    }
}

/// A validator answer the store cannot reconcile with what it asked for.
pub(crate) fn inconsistent(message: impl Into<String>) -> StoreError {
    StoreError::Source(ChainStoreSourceError::inconsistent_data(message))
}
