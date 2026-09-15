//! Adapter error type.
//!
//! Note the classification: a *snapshot acquisition* failure is typed
//! [`Unavailable`](ServeError::Unavailable) (transient), and "no blocks yet" is
//! [`NoBlocks`](ServeError::NoBlocks) (a lifecycle/serviceability fact) — the
//! two are distinct, and neither is recovered by downcasting a transport code.
//! A *broadcast rejection* is not an error at all here: it is a domain answer
//! carried in the `SendResponse` (see `send_transaction`).

use zaino_service::error::Transient;

/// A light-serve handler failure.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    /// The chain has no tip yet — not built to any height.
    #[error("no blocks available yet")]
    NoBlocks,
    /// Could not acquire a coherent snapshot; likely resolves on retry.
    #[error(transparent)]
    Unavailable(#[from] Transient),
}
