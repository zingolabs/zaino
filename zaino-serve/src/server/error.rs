//! Hold error types for the server and related functionality.

use std::io;
use tokio::sync::mpsc::error::TrySendError;

use crate::server::request::ZingoIndexerRequest;
// use zaino_nym::error::NymError;

/// Zingo-Indexer queue errors.
#[derive(Debug, thiserror::Error)]
pub enum QueueError<T> {
    /// Returned when a requests is pushed to a full queue.
    #[error("Queue Full")]
    QueueFull(T),
    /// Returned when a worker or dispatcher tries to receive from an empty queue.
    #[error("Queue Empty")]
    QueueEmpty,
    /// Returned when a worker or dispatcher tries to receive from a closed queue.
    #[error("Queue Disconnected")]
    QueueClosed,
}

/// Zingo-Indexer request errors.
#[derive(Debug, thiserror::Error)]
pub enum RequestError {
    /// Errors originating from incorrect enum types being called.
    #[error("Incorrect variant")]
    IncorrectVariant,
    /// System time errors.
    #[error("System time error: {0}")]
    SystemTimeError(#[from] std::time::SystemTimeError),
    // /// Nym Related Errors
    // #[error("Nym error: {0}")]
    // NymError(#[from] NymError),
}

/// Zingo-Indexer ingestor errors.
#[derive(Debug, thiserror::Error)]
pub enum IngestorError {
    /// Request based errors.
    #[error("Request error: {0}")]
    RequestError(#[from] RequestError),
    // /// Nym based errors.
    // #[error("Nym error: {0}")]
    // NymError(#[from] NymError),
    /// Tcp listener based error.
    #[error("Failed to accept TcpStream: {0}")]
    ClientConnectionError(#[from] io::Error),
    /// Error from failing to send new request to the queue.
    #[error("Failed to send request to the queue: {0}")]
    QueuePushError(#[from] TrySendError<ZingoIndexerRequest>),
}

/// Zingo-Indexer worker errors.
#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    /// Tonic transport error.
    #[error("Tonic transport error: {0}")]
    TonicTransportError(#[from] tonic::transport::Error),
    /// Tokio join error.
    #[error("Tokio join error: {0}")]
    TokioJoinError(#[from] tokio::task::JoinError),
    /// Worker Pool Full.
    #[error("Worker Pool Full")]
    WorkerPoolFull,
    /// Worker Pool at idle.
    #[error("Worker Pool a idle")]
    WorkerPoolIdle,
}

/// Zingo-Indexer server errors.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    /// Request based errors.
    #[error("Request error: {0}")]
    RequestError(#[from] RequestError),
    // /// Nym based errors.
    // #[error("Nym error: {0}")]
    // NymError(#[from] NymError),
    /// Ingestor based errors.
    #[error("Ingestor error: {0}")]
    IngestorError(#[from] IngestorError),
    /// Worker based errors.
    #[error("Worker error: {0}")]
    WorkerError(#[from] WorkerError),
    /// Server configuration errors.
    #[error("Server configuration error: {0}")]
    ServerConfigError(String),
}
