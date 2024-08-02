//! Holds the server worker implementation.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use http::Uri;
use nym_sphinx_anonymous_replies::requests::AnonymousSenderTag;
use tokio::{
    sync::mpsc,
    time::{Duration, Instant},
};
use tonic::transport::Server;

use crate::{
    rpc::GrpcClient,
    server::{error::WorkerError, request::ZingoProxyRequest},
};

#[cfg(not(feature = "nym_poc"))]
use crate::proto::service::compact_tx_streamer_server::CompactTxStreamerServer;

#[cfg(feature = "nym_poc")]
use zcash_client_backend::proto::service::compact_tx_streamer_server::CompactTxStreamerServer;

/// Status of the worker.
#[derive(Debug, Clone, Copy)]
pub enum StatusType {
    /// Running initial startup routine.
    Spawning,
    /// Processing requests from the queue.
    Working,
    /// Waiting for requests from the queue.
    Standby,
    /// Running shutdown routine.
    Closing,
}

/// Wrapper for StatusType that also holds initiation time, used for standby monitoring.
#[derive(Debug, Clone)]
pub enum WorkerStatus {
    /// Running initial startup routine.
    Spawning(Instant),
    /// Processing requests from the queue.
    Working(Instant),
    /// Waiting for requests from the queue.
    Standby(Instant),
    /// Running shutdown routine.
    Closing(Instant),
}

impl WorkerStatus {
    /// Create a new status with the current timestamp.
    pub fn new(status: StatusType) -> WorkerStatus {
        match status {
            StatusType::Spawning => WorkerStatus::Spawning(Instant::now()),
            StatusType::Working => WorkerStatus::Working(Instant::now()),
            StatusType::Standby => WorkerStatus::Standby(Instant::now()),
            StatusType::Closing => WorkerStatus::Closing(Instant::now()),
        }
    }

    /// Return the current status type and the duration the worker has been in this status.
    pub fn status(&self) -> (StatusType, Duration) {
        match self {
            WorkerStatus::Spawning(timestamp) => (StatusType::Spawning, timestamp.elapsed()),
            WorkerStatus::Working(timestamp) => (StatusType::Working, timestamp.elapsed()),
            WorkerStatus::Standby(timestamp) => (StatusType::Standby, timestamp.elapsed()),
            WorkerStatus::Closing(timestamp) => (StatusType::Closing, timestamp.elapsed()),
        }
    }

    /// Update the status to a new one, resetting the timestamp.
    pub fn set(&mut self, new_status: StatusType) {
        *self = match new_status {
            StatusType::Spawning => WorkerStatus::Spawning(Instant::now()),
            StatusType::Working => WorkerStatus::Working(Instant::now()),
            StatusType::Standby => WorkerStatus::Standby(Instant::now()),
            StatusType::Closing => WorkerStatus::Closing(Instant::now()),
        }
    }
}

/// A queue working is the entity that takes requests from the queue and processes them.
///
/// TODO: - Add JsonRpcConnector to worker and pass to underlying RPC services.
///       - Currently a new JsonRpcConnector is spawned for every new RPC serviced.
#[derive(Debug)]
pub struct Worker {
    /// Worker ID.
    worker_id: usize,
    /// Used to pop requests from the queue
    queue: mpsc::Receiver<ZingoProxyRequest>,
    /// Used to requeue requests.
    _requeue: mpsc::Sender<ZingoProxyRequest>,
    /// Used to send responses to the nym_responder.
    nym_responder: mpsc::Sender<(Vec<u8>, AnonymousSenderTag)>,
    /// gRPC client used for processing requests received over http.
    grpc_client: GrpcClient,
    /// Workers current status.
    status: WorkerStatus,
    /// Represents the Online status of the gRPC server.
    pub online: Arc<AtomicBool>,
}

impl Worker {
    /// Creates a new queue worker.
    pub async fn spawn(
        worker_id: usize,
        queue: mpsc::Receiver<ZingoProxyRequest>,
        _requeue: mpsc::Sender<ZingoProxyRequest>,
        nym_responder: mpsc::Sender<(Vec<u8>, AnonymousSenderTag)>,
        lightwalletd_uri: Uri,
        zebrad_uri: Uri,
        online: Arc<AtomicBool>,
    ) -> Result<Self, WorkerError> {
        let grpc_client = GrpcClient {
            lightwalletd_uri,
            zebrad_uri,
            online: online.clone(),
        };
        Ok(Worker {
            worker_id,
            queue,
            _requeue,
            nym_responder,
            grpc_client,
            status: WorkerStatus::new(StatusType::Spawning),
            online,
        })
    }

    /// Starts queue worker service routine.
    ///
    /// TODO: Add requeue on error.
    pub async fn serve(mut self) -> tokio::task::JoinHandle<Result<(), WorkerError>> {
        tokio::task::spawn(async move {
            // NOTE: This interval may need to be reduced or removed / moved once scale testing begins.
            let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(50));
            let svc = CompactTxStreamerServer::new(self.grpc_client.clone());
            // TODO: create tonic server here for use within loop.
            self.status.set(StatusType::Standby);
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        if !self.check_online() {
                            self.status.set(StatusType::Closing);
                            println!("Worker shutting down.");
                            return Ok(());
                        }
                    }
                    incoming = self.queue.recv() => {
                        if !self.check_online() {
                            self.status.set(StatusType::Closing);
                            println!("worker shutting down.");
                            return Ok(());
                        }
                        self.status.set(StatusType::Working);
                        match incoming {
                            Some(ZingoProxyRequest::TcpServerRequest(request)) => {
                                Server::builder().add_service(svc.clone())
                                    .serve_with_incoming( async_stream::stream! {
                                        yield Ok::<_, std::io::Error>(
                                            request.get_request().get_stream()
                                        );
                                    })
                                    .await?;
                            }
                            Some(ZingoProxyRequest::NymServerRequest(request)) => {
                                match self.grpc_client
                                    .process_nym_request(&request)
                                    .await {
                                    Ok(response) => {
                                        if let Err(e) = self.nym_responder.send((response, request.get_request().metadata())).await {
                                            // TODO:: Handle this error!
                                            eprintln!("Failed to send response to nym responder: {}", e);
                                        }
                                    }
                                    Err(e) => {
                                        // TODO:: Handle this error!
                                        eprintln!("Failed to process nym received request: {}", e);

                                    }

                                }
                            }
                            None => {
                                self.status.set(StatusType::Closing);
                                println!("Queue closed, worker shutting down.");
                                return Ok(());
                            }
                        }
                        self.status.set(StatusType::Standby);
                    }
                }
            }
        })
    }

    /// Ends the worker.
    pub async fn shutdown(self) {
        todo!()
    }

    /// Returns the worker's ID.
    pub fn id(&self) -> usize {
        self.worker_id
    }

    /// Returns the workers current status.
    pub fn status(&self) -> (StatusType, Duration) {
        self.status.status()
    }

    /// Check the online status on the server.
    fn check_online(&self) -> bool {
        self.online.load(Ordering::SeqCst)
    }
}
