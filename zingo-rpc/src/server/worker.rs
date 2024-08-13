//! Holds the server worker implementation.

use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};

use http::Uri;
use nym_sphinx_anonymous_replies::requests::AnonymousSenderTag;
use tonic::transport::Server;

use crate::{
    rpc::GrpcClient,
    server::{
        error::{QueueError, WorkerError},
        queue::{QueueReceiver, QueueSender},
        request::ZingoProxyRequest,
        AtomicStatus,
    },
};

#[cfg(not(feature = "nym_poc"))]
use crate::proto::service::compact_tx_streamer_server::CompactTxStreamerServer;

#[cfg(feature = "nym_poc")]
use zcash_client_backend::proto::service::compact_tx_streamer_server::CompactTxStreamerServer;

/// A queue working is the entity that takes requests from the queue and processes them.
///
/// TODO: - Add JsonRpcConnector to worker and pass to underlying RPC services.
///       - Currently a new JsonRpcConnector is spawned for every new RPC serviced.
#[derive(Debug, Clone)]
pub struct Worker {
    /// Worker ID.
    worker_id: usize,
    /// Used to pop requests from the queue
    queue: QueueReceiver<ZingoProxyRequest>,
    /// Used to requeue requests.
    requeue: QueueSender<ZingoProxyRequest>,
    /// Used to send responses to the nym_dispatcher.
    nym_response_queue: QueueSender<(Vec<u8>, AnonymousSenderTag)>,
    /// gRPC client used for processing requests received over http.
    grpc_client: GrpcClient,
    /// Thread safe worker status.
    atomic_status: AtomicStatus,
    /// Represents the Online status of the Worker.
    pub online: Arc<AtomicBool>,
}

impl Worker {
    /// Creates a new queue worker.
    pub async fn spawn(
        worker_id: usize,
        queue: QueueReceiver<ZingoProxyRequest>,
        requeue: QueueSender<ZingoProxyRequest>,
        nym_response_queue: QueueSender<(Vec<u8>, AnonymousSenderTag)>,
        lightwalletd_uri: Uri,
        zebrad_uri: Uri,
        atomic_status: AtomicStatus,
        online: Arc<AtomicBool>,
    ) -> Self {
        let grpc_client = GrpcClient {
            lightwalletd_uri,
            zebrad_uri,
            online: online.clone(),
        };
        Worker {
            worker_id,
            queue,
            requeue,
            nym_response_queue,
            grpc_client,
            atomic_status,
            online,
        }
    }

    /// Starts queue worker service routine.
    ///
    /// TODO: Add requeue logic for node errors.
    pub async fn serve(self) -> tokio::task::JoinHandle<Result<(), WorkerError>> {
        tokio::task::spawn(async move {
            // NOTE: This interval may need to be reduced or removed / moved once scale testing begins.
            let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(100));
            let svc = CompactTxStreamerServer::new(self.grpc_client.clone());
            // TODO: create tonic server here for use within loop.
            self.atomic_status.store(1);
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        if self.check_for_shutdown().await {
                            return Ok(());
                        }
                    }
                    incoming = self.queue.listen() => {
                        match incoming {
                            Ok(request) => {
                                self.atomic_status.store(2);
                                    match request {
                                        ZingoProxyRequest::TcpServerRequest(request) => {
                                            Server::builder().add_service(svc.clone())
                                                .serve_with_incoming( async_stream::stream! {
                                                    yield Ok::<_, std::io::Error>(
                                                        request.get_request().get_stream()
                                                    );
                                                }
                                            )
                                            .await?;
                                        }
                                        ZingoProxyRequest::NymServerRequest(request) => {
                                            match self.grpc_client
                                                .process_nym_request(&request)
                                                .await {
                                                Ok(response) => {
                                                    match self.nym_response_queue.try_send((response, request.get_request().metadata())) {
                                                        Ok(_) => {}
                                                        Err(QueueError::QueueFull(_request)) => {
                                                            eprintln!("Response Queue Full.");
                                                            // TODO: Handle this error! (open second nym responder?).
                                                        }
                                                        Err(e) => {
                                                            self.atomic_status.store(5);
                                                            eprintln!("Response Queue Closed. Failed to send response to queue: {}\nWorker shutting down.", e);
                                                            // TODO: Handle queue closed error here. (return correct error?)
                                                            return Ok(());
                                                        }
                                                    }
                                                }
                                                Err(e) => {
                                                    eprintln!("Failed to process nym received request: {}", e);
                                                    // TODO:: Handle this error!

                                                }

                                            }
                                        }
                                    }
                                // NOTE: This may need to be removed for scale use.
                                if self.check_for_shutdown().await {
                                    self.atomic_status.store(5);
                                    return Ok(());
                                } else {
                                    self.atomic_status.store(1);
                                }
                            }
                            Err(_e) => {
                                self.atomic_status.store(5);
                                eprintln!("Queue closed, worker shutting down.");
                                // TODO: Handle queue closed error here. (return correct error / undate status to correct err code.)
                                return Ok(());
                            }
                        }
                    }
                }
            }
        })
    }

    /// Checks for closure signals.
    ///
    /// Checks AtomicStatus for closure signal.
    /// Checks (online) AtomicBool for fatal error signal.
    pub async fn check_for_shutdown(&self) -> bool {
        if self.atomic_status() >= 4 {
            return true;
        }
        if !self.check_online() {
            return true;
        }
        false
    }

    /// Sets the worker to close gracefully.
    pub async fn shutdown(&mut self) {
        self.atomic_status.store(4)
    }

    /// Returns the worker's ID.
    pub fn id(&self) -> usize {
        self.worker_id
    }

    /// Loads the workers current atomic status.
    pub fn atomic_status(&self) -> usize {
        self.atomic_status.load()
    }

    /// Check the online status on the server.
    fn check_online(&self) -> bool {
        self.online.load(Ordering::SeqCst)
    }
}

/// Holds the status of the worker pool and its workers.
#[derive(Debug, Clone)]
pub struct WorkerPoolStatus {
    workers: Arc<AtomicUsize>,
    statuses: Vec<AtomicStatus>,
}

impl WorkerPoolStatus {
    /// Creates a WorkerPoolStatus.
    pub fn new(max_workers: u16) -> Self {
        WorkerPoolStatus {
            workers: Arc::new(AtomicUsize::new(0)),
            statuses: vec![AtomicStatus::new(5); max_workers as usize],
        }
    }

    /// Returns the WorkerPoolStatus.
    pub fn load(&self) -> WorkerPoolStatus {
        self.workers.load(Ordering::SeqCst);
        for i in 0..self.statuses.len() {
            self.statuses[i].load();
        }
        self.clone()
    }
}

/// Dynamically sized pool of workers.
#[derive(Debug, Clone)]
pub struct WorkerPool {
    /// Maximun number of concurrent workers allowed.
    max_size: u16,
    /// Minimum number of workers kept running on stanby.
    idle_size: u16,
    /// Workers currently in the pool
    workers: Vec<Worker>,
    /// Status of the workerpool and its workers.
    status: WorkerPoolStatus,
    /// Represents the Online status of the WorkerPool.
    pub online: Arc<AtomicBool>,
}

impl WorkerPool {
    /// Creates a new worker pool containing [idle_workers] workers.
    pub async fn spawn(
        max_size: u16,
        idle_size: u16,
        queue: QueueReceiver<ZingoProxyRequest>,
        _requeue: QueueSender<ZingoProxyRequest>,
        nym_response_queue: QueueSender<(Vec<u8>, AnonymousSenderTag)>,
        lightwalletd_uri: Uri,
        zebrad_uri: Uri,
        status: WorkerPoolStatus,
        online: Arc<AtomicBool>,
    ) -> Self {
        let mut workers: Vec<Worker> = Vec::with_capacity(max_size as usize);
        for _ in 0..idle_size {
            workers.push(
                Worker::spawn(
                    workers.len(),
                    queue.clone(),
                    _requeue.clone(),
                    nym_response_queue.clone(),
                    lightwalletd_uri.clone(),
                    zebrad_uri.clone(),
                    status.statuses[workers.len()].clone(),
                    online.clone(),
                )
                .await,
            );
        }
        status.workers.store(idle_size as usize, Ordering::SeqCst);
        WorkerPool {
            max_size,
            idle_size,
            workers,
            status,
            online,
        }
    }

    /// Sets workers in the worker pool to start servicing the queue.
    pub async fn serve(self) -> Vec<tokio::task::JoinHandle<Result<(), WorkerError>>> {
        let mut worker_handles = Vec::new();
        for worker in self.workers {
            worker_handles.push(worker.serve().await);
        }
        worker_handles
    }

    /// Adds a worker to the worker pool, returns error if the pool is already at max size.
    pub async fn push_worker(
        &mut self,
    ) -> Result<tokio::task::JoinHandle<Result<(), WorkerError>>, WorkerError> {
        if self.workers.len() >= self.max_size as usize {
            Err(WorkerError::WorkerPoolFull)
        } else {
            let worker_index = self.workers();
            self.workers.push(
                Worker::spawn(
                    worker_index,
                    self.workers[0].queue.clone(),
                    self.workers[0].requeue.clone(),
                    self.workers[0].nym_response_queue.clone(),
                    self.workers[0].grpc_client.lightwalletd_uri.clone(),
                    self.workers[0].grpc_client.zebrad_uri.clone(),
                    self.status.statuses[worker_index].clone(),
                    self.online.clone(),
                )
                .await,
            );
            self.status.workers.fetch_add(1, Ordering::SeqCst);
            Ok(self.workers[worker_index].clone().serve().await)
        }
    }

    /// Removes a worker from the worker pool, returns error if the pool is already at idle size.
    pub async fn pop_worker(
        &mut self,
        worker_handle: tokio::task::JoinHandle<Result<(), WorkerError>>,
    ) -> Result<(), WorkerError> {
        if self.workers.len() <= self.idle_size as usize {
            Err(WorkerError::WorkerPoolIdle)
        } else {
            let worker_index = self.workers.len() - 1;
            self.workers[worker_index].shutdown().await;
            match worker_handle.await {
                Ok(worker) => match worker {
                    Ok(()) => {
                        self.status.statuses[worker_index].store(5);
                        self.workers.pop();
                        self.status.workers.fetch_sub(1, Ordering::SeqCst);
                        return Ok(());
                    }
                    Err(e) => {
                        self.status.statuses[worker_index].store(6);
                        eprintln!("Worker returned error on shutdown: {}", e);
                        // TODO: Handle the inner WorkerError. Return error.
                        self.status.workers.fetch_sub(1, Ordering::SeqCst);
                        return Ok(());
                    }
                },
                Err(e) => {
                    self.status.statuses[worker_index].store(6);
                    eprintln!("Worker returned error on shutdown: {}", e);
                    // TODO: Handle the JoinError. Return error.
                    self.status.workers.fetch_sub(1, Ordering::SeqCst);
                    return Ok(());
                }
            };
        }
    }

    /// Returns the max size of the pool
    pub fn max_size(&self) -> u16 {
        self.max_size
    }

    /// Returns the idle size of the pool
    pub fn idle_size(&self) -> u16 {
        self.idle_size
    }

    /// Returns the current number of workers in the pool.
    pub fn workers(&self) -> usize {
        self.workers.len()
    }

    /// Fetches and returns the status of the workerpool and its workers.
    pub fn status(&self) -> WorkerPoolStatus {
        self.status.workers.load(Ordering::SeqCst);
        for i in 0..self.workers() {
            self.status.statuses[i].load();
        }
        self.status.clone()
    }

    /// Shuts down all the workers in the pool.
    pub async fn shutdown(
        &mut self,
        worker_handles: &mut Vec<Option<tokio::task::JoinHandle<Result<(), WorkerError>>>>,
    ) {
        for i in (0..self.workers.len()).rev() {
            self.workers[i].shutdown().await;
            if let Some(worker_handle) = worker_handles[i].take() {
                match worker_handle.await {
                    Ok(worker) => match worker {
                        Ok(()) => {
                            self.status.statuses[i].store(5);
                            self.workers.pop();
                            self.status.workers.fetch_sub(1, Ordering::SeqCst);
                        }
                        Err(e) => {
                            self.status.statuses[i].store(6);
                            eprintln!("Worker returned error on shutdown: {}", e);
                            // TODO: Handle the inner WorkerError
                            self.status.workers.fetch_sub(1, Ordering::SeqCst);
                        }
                    },
                    Err(e) => {
                        self.status.statuses[i].store(6);
                        eprintln!("Worker returned error on shutdown: {}", e);
                        // TODO: Handle the JoinError
                        self.status.workers.fetch_sub(1, Ordering::SeqCst);
                    }
                };
            }
        }
    }
}
