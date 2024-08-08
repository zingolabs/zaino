//! Zingo-Proxy client server.

use http::Uri;
use nym_sphinx_anonymous_replies::requests::AnonymousSenderTag;
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
};

pub mod dispatcher;
pub mod error;
pub mod ingestor;
pub mod queue;
pub mod request;
pub mod worker;

use self::{
    dispatcher::NymDispatcher,
    error::{DispatcherError, IngestorError, ServerError, WorkerError},
    ingestor::{NymIngestor, TcpIngestor},
    queue::Queue,
    request::ZingoProxyRequest,
    worker::{WorkerPool, WorkerPoolStatus},
};

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
    pub fn new(status: usize) -> Self {
        Self(Arc::new(AtomicUsize::new(status)))
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

/// Status of the server.
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

/// Holds the status of the server and all its components.
#[derive(Debug, Clone)]
pub struct ServerStatus {
    server_status: AtomicStatus,
    tcp_ingestor_status: AtomicStatus,
    nym_ingestor_status: AtomicStatus,
    nym_dispatcher_status: AtomicStatus,
    workerpool_status: WorkerPoolStatus,
    request_queue_status: Arc<AtomicUsize>,
    nym_response_queue_status: Arc<AtomicUsize>,
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
    nym_response_queue: Queue<(Vec<u8>, AnonymousSenderTag)>,
    /// Servers current status.
    status: ServerStatus,
    /// Represents the Online status of the Server.
    pub online: Arc<AtomicBool>,
}

impl Server {
    /// Spawns a new Server.
    pub async fn spawn(
        tcp_active: bool,
        tcp_ingestor_listen_addr: Option<SocketAddr>,
        nym_active: bool,
        nym_conf_path: Option<&str>,
        lightwalletd_uri: Uri,
        zebrad_uri: Uri,
        max_queue_size: usize,
        max_worker_pool_size: usize,
        idle_worker_pool_size: usize,
        status: ServerStatus,
        online: Arc<AtomicBool>,
    ) -> Result<Self, ServerError> {
        if !(tcp_active && nym_active) {
            return Err(ServerError::ServerConfigError(
                "Cannot start server with no ingestors selected, at least one of either nym or tcp must be set to active in conf.".to_string(),
            ));
        }
        if tcp_active && tcp_ingestor_listen_addr.is_none() {
            return Err(ServerError::ServerConfigError(
                "TCP is active but no address provided.".to_string(),
            ));
        }
        if nym_active && nym_conf_path.is_none() {
            return Err(ServerError::ServerConfigError(
                "NYM is active but no conf path provided.".to_string(),
            ));
        }
        status.server_status.store(0);
        let request_queue: Queue<ZingoProxyRequest> =
            Queue::new(max_queue_size, status.request_queue_status.clone());
        status.request_queue_status.store(0, Ordering::SeqCst);
        let nym_response_queue: Queue<(Vec<u8>, AnonymousSenderTag)> =
            Queue::new(max_queue_size, status.nym_response_queue_status.clone());
        status.nym_response_queue_status.store(0, Ordering::SeqCst);
        let tcp_ingestor = if tcp_active {
            Some(
                TcpIngestor::spawn(
                    tcp_ingestor_listen_addr.expect(
                        "tcp_ingestor_listen_addr returned none when used, after checks made.",
                    ),
                    request_queue.tx().clone(),
                    status.tcp_ingestor_status.clone(),
                    online.clone(),
                )
                .await?,
            )
        } else {
            None
        };
        let nym_ingestor = if nym_active {
            Some(
                NymIngestor::spawn(
                    nym_conf_path
                        .expect("nym_conf_path returned none when used, after checks made."),
                    request_queue.tx().clone(),
                    status.nym_ingestor_status.clone(),
                    online.clone(),
                )
                .await?,
            )
        } else {
            None
        };
        let nym_dispatcher = if nym_active {
            Some(
                NymDispatcher::spawn(
                    nym_conf_path
                        .expect("nym_conf_path returned none when used, after checks made."),
                    nym_response_queue.rx().clone(),
                    nym_response_queue.tx().clone(),
                    status.nym_dispatcher_status.clone(),
                    online.clone(),
                )
                .await?,
            )
        } else {
            None
        };
        let worker_pool = WorkerPool::spawn(
            max_worker_pool_size,
            idle_worker_pool_size,
            request_queue.rx().clone(),
            request_queue.tx().clone(),
            nym_response_queue.tx().clone(),
            lightwalletd_uri,
            zebrad_uri,
            status.workerpool_status.clone(),
            online.clone(),
        )
        .await;
        Ok(Server {
            tcp_ingestor,
            nym_ingestor,
            nym_dispatcher,
            worker_pool,
            request_queue,
            nym_response_queue,
            status: status.clone(),
            online,
        })
    }

    /// Starts the gRPC service.
    ///
    /// Launches all components then enters command loop:
    /// - Checks request queue and workerpool to spawn / despawn workers as required.
    /// - Updates the ServerStatus.
    /// - Checks for shutdown signal, shutting down server if received.
    pub async fn serve(mut self) -> tokio::task::JoinHandle<Result<(), ServerError>> {
        tokio::task::spawn(async move {
            // NOTE: This interval may need to be reduced or removed / moved once scale testing begins.
            let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(50));
            let mut nym_dispatcher_handle = None;
            let mut nym_ingestor_handle = None;
            let mut tcp_ingestor_handle = None;
            let mut worker_handles;
            if let Some(dispatcher) = self.nym_dispatcher.take() {
                nym_dispatcher_handle = Some(dispatcher.serve().await);
            }
            if let Some(ingestor) = self.nym_ingestor.take() {
                nym_ingestor_handle = Some(ingestor.serve().await);
            }
            if let Some(ingestor) = self.tcp_ingestor.take() {
                tcp_ingestor_handle = Some(ingestor.serve().await);
            }
            worker_handles = self.worker_pool.clone().serve().await;
            self.status.server_status.store(1);
            loop {
                if self.request_queue.queue_length() >= (self.request_queue.max_length() / 2)
                    && (self.worker_pool.workers() < self.worker_pool.max_size())
                {
                    match self.worker_pool.push_worker().await {
                        Ok(handle) => {
                            worker_handles.push(handle);
                        }
                        Err(_e) => {
                            eprintln!("WorkerPool at capacity");
                        }
                    }
                } else if (self.request_queue.queue_length() <= 1)
                    && (self.worker_pool.workers() > self.worker_pool.idle_size())
                {
                    let worker_index = self.worker_pool.workers() - 1;
                    let worker_handle = worker_handles.remove(worker_index);
                    match self.worker_pool.pop_worker(worker_handle).await {
                        Ok(_) => {}
                        Err(e) => {
                            eprintln!("Failed to pop worker from pool: {}", e);
                            // TODO: Handle this error.
                        }
                    }
                }
                self.statuses();
                // TODO: Implement check_statuses() and run here.
                if self.check_for_shutdown().await {
                    let worker_handle_options: Vec<
                        Option<tokio::task::JoinHandle<Result<(), WorkerError>>>,
                    > = worker_handles.into_iter().map(Some).collect();
                    self.shutdown_components(
                        tcp_ingestor_handle,
                        nym_ingestor_handle,
                        nym_dispatcher_handle,
                        worker_handle_options,
                    )
                    .await;
                    self.status.server_status.store(5);
                    return Ok(());
                }
                interval.tick().await;
            }
        })
    }

    /// Checks indexers online status and servers internal status for closure signal.
    pub async fn check_for_shutdown(&self) -> bool {
        if self.status() >= 4 {
            return true;
        }
        if !self.check_online() {
            return true;
        }
        false
    }

    /// Sets the servers to close gracefully.
    pub async fn shutdown(&mut self) {
        self.status.server_status.store(4)
    }

    /// Sets the server's components to close gracefully.
    async fn shutdown_components(
        &mut self,
        tcp_ingestor_handle: Option<tokio::task::JoinHandle<Result<(), IngestorError>>>,
        nym_ingestor_handle: Option<tokio::task::JoinHandle<Result<(), IngestorError>>>,
        nym_dispatcher_handle: Option<tokio::task::JoinHandle<Result<(), DispatcherError>>>,
        mut worker_handles: Vec<Option<tokio::task::JoinHandle<Result<(), WorkerError>>>>,
    ) {
        if let Some(handle) = tcp_ingestor_handle {
            self.status.tcp_ingestor_status.store(4);
            handle.await.ok();
        }
        if let Some(handle) = nym_ingestor_handle {
            self.status.nym_ingestor_status.store(4);
            handle.await.ok();
        }
        self.worker_pool.shutdown(&mut worker_handles).await;
        if let Some(handle) = nym_dispatcher_handle {
            self.status.nym_dispatcher_status.store(4);
            handle.await.ok();
        }
        self.online
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }

    /// Returns the servers current status usize.
    pub fn status(&self) -> usize {
        self.status.server_status.load()
    }

    /// Returns the servers current statustype.
    pub fn statustype(&self) -> StatusType {
        StatusType::from(self.status())
    }

    /// Updates and returns the status of the server and its parts.
    pub fn statuses(&mut self) -> ServerStatus {
        self.status.server_status.load();
        self.status.tcp_ingestor_status.load();
        self.status.nym_ingestor_status.load();
        self.status.nym_dispatcher_status.load();
        self.status
            .request_queue_status
            .store(self.request_queue.queue_length(), Ordering::SeqCst);
        self.status
            .nym_response_queue_status
            .store(self.nym_response_queue.queue_length(), Ordering::SeqCst);
        self.worker_pool.status();
        self.status.clone()
    }

    /// Checks statuses, handling errors.
    pub async fn check_statuses(&mut self) {
        todo!()
    }

    /// Check the online status on the indexer.
    fn check_online(&self) -> bool {
        self.online.load(Ordering::SeqCst)
    }
}
