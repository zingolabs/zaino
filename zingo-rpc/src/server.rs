//! Zingo-Proxy client server.

use nym_sphinx_anonymous_replies::requests::AnonymousSenderTag;
use std::sync::{atomic::AtomicBool, Arc};

pub mod dispatcher;
pub mod error;
pub mod ingestor;
pub mod queue;
pub mod request;
pub mod worker;
pub mod workerpool;

use self::{
    dispatcher::NymDispatcher,
    ingestor::{NymIngestor, TcpIngestor},
    queue::Queue,
    request::ZingoProxyRequest,
    workerpool::WorkerPool,
};

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
