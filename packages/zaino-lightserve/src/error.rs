//! Adapter error type.
//!
//! Note the classification: a *snapshot acquisition* failure is typed
//! [`Unavailable`](ServeError::Unavailable) (transient), and "no blocks yet" is
//! [`NoBlocks`](ServeError::NoBlocks) (a lifecycle/serviceability fact) — the
//! two are distinct, and neither is recovered by downcasting a transport code.
//! A *broadcast rejection* is not an error at all here: it is a domain answer
//! carried in the `SendResponse` (see `send_transaction`).

use zaino_core::Capability;
use zaino_service::error::{BlockReadError, ReadError, Transient, TreestateReadError};

/// A light-serve handler failure.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    /// The chain has no tip yet — not built to any height.
    #[error("no blocks available yet")]
    NoBlocks,
    /// A requested read is not serviceable yet — its backing index is not built
    /// to that height. A serviceability fact, not a transport failure.
    #[error("not serviceable yet: {0:?}")]
    NotServiceable(Capability),
    /// Could not acquire a coherent snapshot; likely resolves on retry.
    #[error(transparent)]
    Unavailable(#[from] Transient),
    /// Unrecoverable backend failure while serving the read.
    #[error("internal serve failure: {0}")]
    Internal(String),
}

// The read errors carry a `NotServiceable | Transient | Fatal` kind; preserve
// that kind rather than fusing it into one transport code. Exhaustive matches,
// so a new read-error variant must be classified here.
impl From<BlockReadError> for ServeError {
    fn from(err: BlockReadError) -> Self {
        match err {
            BlockReadError::NotServiceable(cap) => ServeError::NotServiceable(cap),
            BlockReadError::Transient(msg) => ServeError::Unavailable(Transient(msg)),
            BlockReadError::Fatal(msg) => ServeError::Internal(msg),
        }
    }
}

impl From<ReadError> for ServeError {
    fn from(err: ReadError) -> Self {
        match err {
            ReadError::NotServiceable(cap) => ServeError::NotServiceable(cap),
            ReadError::Transient(msg) => ServeError::Unavailable(Transient(msg)),
            ReadError::Fatal(msg) => ServeError::Internal(msg),
        }
    }
}

impl From<TreestateReadError> for ServeError {
    fn from(err: TreestateReadError) -> Self {
        match err {
            TreestateReadError::NotServiceable(cap) => ServeError::NotServiceable(cap),
            TreestateReadError::Transient(msg) => ServeError::Unavailable(Transient(msg)),
            TreestateReadError::Fatal(msg) => ServeError::Internal(msg),
        }
    }
}
