//! Zingo-Proxy client server.

use nym_sphinx_anonymous_replies::requests::AnonymousSenderTag;
use std::sync::{atomic::AtomicBool, Arc};
use tokio::sync::mpsc;

use self::{
    dispatcher::NymDispatcher,
    ingestor::{NymIngestor, TcpIngestor},
    request::ZingoProxyRequest,
    worker::WorkerPool,
};

pub mod dispatcher;
pub mod error;
pub mod ingestor;
pub mod request;
pub mod worker;

///
pub struct Queue<T> {
    /// Maximum length of the queue.
    max_size: usize,
    /// Queue sender.
    queue_tx: mpsc::Sender<T>,
    /// Queue receiver.
    queue_rx: mpsc::Receiver<T>,
}

/// LightWallet server capable of servicing clients over both http and nym.
pub struct Server {
    /// Listens for incoming gRPC requests over HTTP.
    tcp_ingestor: TcpIngestor,
    /// Listens for incoming gRPC requests over Nym Mixnet.
    nym_ingestor: NymIngestor,
    /// Sends gRPC responses over Nym Mixnet.
    nym_dispatcher: NymDispatcher,
    /// Dynamically sized pool of workers.
    worker_pool: WorkerPool,
    /// Request queue.
    request_queue: Queue<ZingoProxyRequest>,
    /// Nym response queue.
    nym_response_queue: Queue<(Vec<u8>, AnonymousSenderTag)>,
    /// Represents the Online status of the Server.
    pub online: Arc<AtomicBool>,
}
