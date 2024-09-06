//! Zaino's gRPC server implementation.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

pub mod director;
pub mod error;
pub(crate) mod ingestor;
pub(crate) mod queue;
pub mod request;
pub(crate) mod worker;

/// Holds a thread safe reperesentation of a StatusType.
/// Possible values:
/// - [0: Spawning]
/// - [1: Listening]
/// - [2: Working]
/// - [3: Inactive]
/// - [4: Closing].
/// - [>=5: Offline].
/// - [>=6: Error].
/// TODO: Define error code spec.
#[derive(Debug, Clone)]
pub struct AtomicStatus(Arc<AtomicUsize>);

impl AtomicStatus {
    /// Creates a new AtomicStatus
    pub fn new(status: u16) -> Self {
        Self(Arc::new(AtomicUsize::new(status as usize)))
    }

    /// Loads the value held in the AtomicStatus
    pub fn load(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }

    /// Sets the value held in the AtomicStatus
    pub fn store(&self, status: usize) {
        self.0.store(status, Ordering::SeqCst);
    }
}

/// Status of the server's components.
#[derive(Debug, PartialEq, Clone)]
pub enum StatusType {
    /// Running initial startup routine.
    Spawning = 0,
    /// Waiting for requests from the queue.
    Listening = 1,
    /// Processing requests from the queue.StatusType
    Working = 2,
    /// On hold, due to blockcache / node error.
    Inactive = 3,
    /// Running shutdown routine.
    Closing = 4,
    /// Offline.
    Offline = 5,
    /// Offline.
    Error = 6,
}

impl From<usize> for StatusType {
    fn from(value: usize) -> Self {
        match value {
            0 => StatusType::Spawning,
            1 => StatusType::Listening,
            2 => StatusType::Working,
            3 => StatusType::Inactive,
            4 => StatusType::Closing,
            5 => StatusType::Offline,
            _ => StatusType::Error,
        }
    }
}

impl From<StatusType> for usize {
    fn from(status: StatusType) -> Self {
        status as usize
    }
}

impl From<AtomicStatus> for StatusType {
    fn from(status: AtomicStatus) -> Self {
        status.load().into()
    }
}
