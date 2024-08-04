//! Zingo-Proxy client server.

use nym_sphinx_anonymous_replies::requests::AnonymousSenderTag;
use std::sync::{atomic::AtomicBool, Arc};
use tokio::sync::mpsc;

use self::{
    dispatcher::NymDispatcher,
    ingestor::{NymIngestor, TcpIngestor},
    request::ZingoProxyRequest,
    worker::Worker,
};

pub mod dispatcher;
pub mod error;
pub mod ingestor;
pub mod request;
pub mod worker;

/// Queue with max length.
pub struct Queue<T> {
    /// Maximum length of the queue.
    max_size: usize,
    /// Queue sender.
    queue_tx: mpsc::Sender<T>,
    /// Queue receiver.
    queue_rx: mpsc::Receiver<T>,
}

impl<T> Queue<T> {
    // Creates a new queue
    // pub fn spawn(max_size) -> Self {}

    // Returns a queue transmitter
    // pub fn tx(&self) -> mpsc::Sender<T> {}

    // Returns a queue receiver
    // pub fn rx(&self) -> mpsc::Receiver<T> {}

    // Returns the current length of the queue
    // pub fn length(&self) -> usize {}
}

/// Dynamically sized pool of workers.
pub struct WorkerPool {
    /// Maximun number of concurrent workers allowed.
    max_size: usize,
    /// Minimum number of workers kept running on stanby.
    idle_size: usize,
    /// Workers currently in the pool
    workers: Vec<Worker>,
    /// Represents the Online status of the WorkerPool.
    pub online: Arc<AtomicBool>,
}

impl WorkerPool {
    // Creates a new worker pool with idle_workers in it.
    // pub fn spawn(max_size, idle_size, online) -> Self {}

    // Sets workers in the worker pool to start servicing the queue.
    // pub fn serve(&self) -> Vec<tokio::task::JoinHandle<Result<(), WorkerError>>> {}

    // Adds a worker to the worker pool, returns error if the pool is already at max size.
    // pub fn add_worker(&self) -> tokio::task::JoinHandle<Result<(), WorkerError>> {}

    // Checks workers on standby, closes workers that have been on standby for longer than 30s(may need to change).
    // pub fn check_workers(&self) {}
}

/// LightWallet server capable of servicing clients over both http and nym.
pub struct Server {
    /// Listens for incoming gRPC requests over HTTP.
    tcp_ingestor: Option<TcpIngestor>,
    /// Listens for incoming gRPC requests over Nym Mixnet.
    nym_ingestor: Option<NymIngestor>,
    /// Sends gRPC responses over Nym Mixnet.
    nym_dispatcher: Option<NymDispatcher>,
    /// Dynamically sized pool of workers.
    worker_pool: WorkerPool,
    /// Request queue.
    request_queue: Queue<ZingoProxyRequest>,
    /// Nym response queue.
    nym_response_queue: Option<Queue<(Vec<u8>, AnonymousSenderTag)>>,
    /// Represents the Online status of the Server.
    pub online: Arc<AtomicBool>,
}

impl Server {
    // Spawns a new server.
    // pub fn Spawn() -> Self {}

    // Starts the server.
    // pub fn Start(&self) {}

    // Returns the status of the server and its parts, to be consumed by system printout.
    // pub fn Status(&self) {}
}
