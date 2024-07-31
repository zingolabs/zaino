//! Holds the queue worker implementation.

use tokio::sync::mpsc;

use super::request::ZingoProxyRequest;

/// Status of the worker.
///
/// TODO: Add duration to each variant.
#[derive(Debug, Clone)]
pub enum WorkerStatus {
    /// Running initial startup routine.
    Spawning,
    /// Processing requests from the queue.
    Working,
    /// Waiting for requests from the queue.
    Standby,
    /// Running shutdown routine.
    Closing,
}

/// A queue working is the entity that takes requests from the queue and processes them.
///
/// TODO: - Add JsonRpcConnector to worker and use by RPC services.
///       - Currently a new JsonRpcConnector is spawned for every RPC serviced.
#[derive(Debug)]
pub struct Worker {
    /// Worker ID.
    worker_id: u16,
    /// Workers current status.
    status: WorkerStatus,
    /// Used to pop requests from the queue
    queue_receiver: mpsc::Receiver<ZingoProxyRequest>,
    /// Used to requeue requests.
    queue_sender: mpsc::Sender<ZingoProxyRequest>,
    // /// Nym Client used to return responses for requests received over nym.
    // nym_client:
    // /// Tonic server used for processing requests received over http.
    // grpc_client:
}

impl Worker {
    /// Creates a new queue worker.
    pub async fn new() -> Self {
        todo!()
    }

    /// Starts queue workers service routine.
    pub async fn serve(&self) {
        todo!()
    }

    /// Ends the worker.
    pub async fn shutdown(self) {
        todo!()
    }

    /// Returns the worker's ID.
    pub fn id(&self) -> u16 {
        self.worker_id
    }

    /// Returns the workers current status.
    pub fn status(&self) -> WorkerStatus {
        self.status.clone()
    }
}
