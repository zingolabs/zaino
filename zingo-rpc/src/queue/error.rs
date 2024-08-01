//! Hold error types for the queue and related functionality.

use std::io;
use tokio::sync::mpsc::error::TrySendError;

use crate::{nym::error::NymError, queue::request::ZingoProxyRequest};

/// Zingo-Proxy request errors.
#[derive(Debug, thiserror::Error)]
pub enum RequestError {
    /// Errors originating from incorrect enum types being called.
    #[error("Incorrect variant")]
    IncorrectVariant,
    /// System time errors.
    #[error("System time error: {0}")]
    SystemTimeError(#[from] std::time::SystemTimeError),
    /// Nym Related Errors
    #[error("Nym error: {0}")]
    NymError(#[from] NymError),
}

/// Zingo-Proxy ingestor errors.
#[derive(Debug, thiserror::Error)]
pub enum IngestorError {
    /// Request based errors.
    #[error("Request error: {0}")]
    RequestError(#[from] RequestError),
    /// Nym based errors.
    #[error("Nym error: {0}")]
    NymError(#[from] NymError),
    /// Tcp listener based error.
    #[error("Failed to accept TcpStream: {0}")]
    ClientConnectionError(#[from] io::Error),
    /// Error from failing to send new request to the queue.
    #[error("Failed to send request to the queue: {0}")]
    QueuePushError(#[from] TrySendError<ZingoProxyRequest>),
}

/// Zingo-Proxy worker errors.
#[derive(Debug, thiserror::Error)]
pub enum WorkerError {}
