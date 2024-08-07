//! Holds the server worker implementation.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use http::Uri;
use nym_sphinx_anonymous_replies::requests::AnonymousSenderTag;
use tokio::time::{Duration, Instant};
use tonic::transport::Server;

use crate::{
    rpc::GrpcClient,
    server::{
        error::{QueueError, WorkerError},
        queue::{QueueReceiver, QueueSender},
        request::ZingoProxyRequest,
    },
};

#[cfg(not(feature = "nym_poc"))]
use crate::proto::service::compact_tx_streamer_server::CompactTxStreamerServer;

#[cfg(feature = "nym_poc")]
use zcash_client_backend::proto::service::compact_tx_streamer_server::CompactTxStreamerServer;

/// Status of the worker.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum WorkerStatusType {
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
    pub fn new(status: WorkerStatusType) -> WorkerStatus {
        match status {
            WorkerStatusType::Spawning => WorkerStatus::Spawning(Instant::now()),
            WorkerStatusType::Working => WorkerStatus::Working(Instant::now()),
            WorkerStatusType::Standby => WorkerStatus::Standby(Instant::now()),
            WorkerStatusType::Closing => WorkerStatus::Closing(Instant::now()),
        }
    }

    /// Return the current status type and the duration the worker has been in this status.
    pub fn status(&self) -> (WorkerStatusType, Duration) {
        match self {
            WorkerStatus::Spawning(timestamp) => (WorkerStatusType::Spawning, timestamp.elapsed()),
            WorkerStatus::Working(timestamp) => (WorkerStatusType::Working, timestamp.elapsed()),
            WorkerStatus::Standby(timestamp) => (WorkerStatusType::Standby, timestamp.elapsed()),
            WorkerStatus::Closing(timestamp) => (WorkerStatusType::Closing, timestamp.elapsed()),
        }
    }

    /// Update the status to a new one, resetting the timestamp.
    pub fn set(&mut self, new_status: WorkerStatusType) {
        *self = match new_status {
            WorkerStatusType::Spawning => WorkerStatus::Spawning(Instant::now()),
            WorkerStatusType::Working => WorkerStatus::Working(Instant::now()),
            WorkerStatusType::Standby => WorkerStatus::Standby(Instant::now()),
            WorkerStatusType::Closing => WorkerStatus::Closing(Instant::now()),
        }
    }
}

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
    /// Workers current status.
    status: WorkerStatus,
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
            status: WorkerStatus::new(WorkerStatusType::Spawning),
            online,
        }
    }

    /// Starts queue worker service routine.
    ///
    /// TODO: Add requeue logic for node errors.
    pub async fn serve(mut self) -> tokio::task::JoinHandle<Result<(), WorkerError>> {
        tokio::task::spawn(async move {
            // NOTE: This interval may need to be reduced or removed / moved once scale testing begins.
            let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(50));
            let svc = CompactTxStreamerServer::new(self.grpc_client.clone());
            // TODO: create tonic server here for use within loop.
            self.status.set(WorkerStatusType::Standby);
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
                                // NOTE: This may need to be removed / moved for scale use (possible it should be moved to after the request is serviced?).
                                if self.check_for_shutdown().await {
                                    match self.requeue.try_send(request) {
                                        Ok(_) => {
                                            return Ok(());
                                        }
                                        Err(QueueError::QueueFull(_request)) => {
                                            eprintln!("Request Queue Full. Failed to send response to queue.\nWorker shutting down.");
                                            // TODO: Handle this error! (cancel shutdown?).
                                            return Ok(());
                                        }
                                        Err(e) => {
                                            self.status.set(WorkerStatusType::Closing);
                                            eprintln!("Request Queue Closed. Failed to send response to queue: {}\nWorker shutting down.", e);
                                            // TODO: Handle queue closed error here. (return correct error?)
                                            return Ok(());
                                        }
                                    }
                                } else {
                                    self.status.set(WorkerStatusType::Working);
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
                                                            self.status.set(WorkerStatusType::Closing);
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
                                    self.status.set(WorkerStatusType::Standby);
                                }
                            }
                            Err(_e) => {
                                self.status.set(WorkerStatusType::Closing);
                                eprintln!("Queue closed, worker shutting down.");
                                // TODO: Handle queue closed error here. (return correct error?)
                                return Ok(());
                            }
                        }
                    }
                }
            }
        })
    }

    /// Checks indexers online status and workers internal status for closure signal.
    pub async fn check_for_shutdown(&self) -> bool {
        if let WorkerStatus::Closing(_) = self.status {
            return true;
        }
        if !self.check_online() {
            return true;
        }
        return false;
    }

    /// Sets the worker to close gracefully.
    pub async fn shutdown(&mut self) {
        self.status.set(WorkerStatusType::Closing)
    }

    /// Returns the worker's ID.
    pub fn id(&self) -> usize {
        self.worker_id
    }

    /// Returns the workers current status.
    pub fn status(&self) -> (WorkerStatusType, Duration) {
        self.status.status()
    }

    /// Check the online status on the server.
    fn check_online(&self) -> bool {
        self.online.load(Ordering::SeqCst)
    }
}

/// Dynamically sized pool of workers.
#[derive(Debug, Clone)]
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
    /// Creates a new worker pool containing [idle_workers] workers.
    pub async fn spawn(
        max_size: usize,
        idle_size: usize,
        queue: QueueReceiver<ZingoProxyRequest>,
        _requeue: QueueSender<ZingoProxyRequest>,
        nym_response_queue: QueueSender<(Vec<u8>, AnonymousSenderTag)>,
        lightwalletd_uri: Uri,
        zebrad_uri: Uri,
        online: Arc<AtomicBool>,
    ) -> Self {
        let mut workers: Vec<Worker> = Vec::with_capacity(max_size);
        for _ in 0..idle_size {
            workers.push(
                Worker::spawn(
                    workers.len(),
                    queue.clone(),
                    _requeue.clone(),
                    nym_response_queue.clone(),
                    lightwalletd_uri.clone(),
                    zebrad_uri.clone(),
                    online.clone(),
                )
                .await,
            );
        }

        WorkerPool {
            max_size,
            idle_size,
            workers,
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
        if self.workers.len() >= self.max_size {
            Err(WorkerError::WorkerPoolFull)
        } else {
            self.workers.push(
                Worker::spawn(
                    self.workers.len(),
                    self.workers[0].queue.clone(),
                    self.workers[0].requeue.clone(),
                    self.workers[0].nym_response_queue.clone(),
                    self.workers[0].grpc_client.lightwalletd_uri.clone(),
                    self.workers[0].grpc_client.zebrad_uri.clone(),
                    self.online.clone(),
                )
                .await,
            );
            Ok(self.workers[self.workers.len()].clone().serve().await)
        }
    }

    /// Removes a worker from the worker pool, returns error if the pool is already at idle size.
    pub async fn pop_worker(
        &mut self,
        worker_handle: tokio::task::JoinHandle<Result<(), WorkerError>>,
    ) -> Result<(), WorkerError> {
        if self.workers.len() <= self.idle_size {
            Err(WorkerError::WorkerPoolIdle)
        } else {
            let worker_index = self.workers.len() - 1;
            self.workers[worker_index].shutdown().await;
            match worker_handle.await {
                Ok(worker) => match worker {
                    Ok(()) => {
                        self.workers.pop();
                        return Ok(());
                    }
                    Err(e) => {
                        eprintln!("Worker returned error on shutdown: {}", e);
                        // TODO: Handle the inner WorkerError
                        return Ok(());
                    }
                },
                Err(e) => {
                    eprintln!("Worker returned error on shutdown: {}", e);
                    // TODO: Handle the JoinError
                    return Ok(());
                }
            };
        }
    }

    /// Returns the max size of the pool
    pub fn max_size(&self) -> usize {
        self.max_size
    }

    /// Returns the idle size of the pool
    pub fn idle_size(&self) -> usize {
        self.idle_size
    }

    /// Returns the current number of workers in the pool.
    pub fn workers(&self) -> usize {
        self.workers.len()
    }

    /// Returns the statuses of all the workers in the workerpool.
    pub fn statuses(&self) -> Vec<(WorkerStatusType, Duration)> {
        let mut worker_statuses = Vec::new();
        for i in 0..self.workers.len() {
            worker_statuses.push(self.workers[i].status())
        }
        worker_statuses
    }

    /// Returns the number of workers in Standby mode for 30 seconds or longer.
    pub fn check_long_standby(&self) -> usize {
        let statuses = self.statuses();
        statuses
            .iter()
            .filter(|(status, duration)| {
                *status == WorkerStatusType::Standby && *duration >= Duration::from_secs(30)
            })
            .count()
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
                            self.workers.pop();
                        }
                        Err(e) => {
                            eprintln!("Worker returned error on shutdown: {}", e);
                            // TODO: Handle the inner WorkerError
                        }
                    },
                    Err(e) => {
                        eprintln!("Worker returned error on shutdown: {}", e);
                        // TODO: Handle the JoinError
                    }
                };
            }
        }
    }
}
