//! Zingo-Proxy client server.

use http::Uri;
use nym_sphinx_anonymous_replies::requests::AnonymousSenderTag;
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
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
    dispatcher::{DispatcherStatus, NymDispatcher},
    error::{DispatcherError, IngestorError, ServerError, WorkerError},
    ingestor::{IngestorStatus, NymIngestor, TcpIngestor},
    queue::Queue,
    request::ZingoProxyRequest,
    worker::{WorkerPool, WorkerStatusType},
};

///
#[derive(Debug, PartialEq, Clone)]
pub struct ServerStatus {
    tcp_ingestor_status: IngestorStatus,
    nym_ingestor_status: IngestorStatus,
    nym_dispatcher_status: DispatcherStatus,
    workerpool_status: (usize, usize, Vec<WorkerStatusType>),
    request_queue_status: (usize, usize),
    nym_response_queue_status: (usize, usize),
}
/// Status of the server.
#[derive(Debug, PartialEq, Clone)]
pub enum ServerStatusType {
    /// Running initial startup routine.
    Spawning(ServerStatus),
    /// Processing incoming requests.
    Active(ServerStatus),
    /// Waiting for node / blockcache to sync.
    Hold(ServerStatus),
    /// Running shutdown routine.
    Closing(ServerStatus),
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
    status: ServerStatusType,
    /// Represents the Online status of the Server.
    pub online: Arc<AtomicBool>,
}

impl Server {
    /// Spawns a new server.
    pub async fn spawn(
        tcp_active: bool,
        tcp_ingestor_listen_addr: SocketAddr,
        nym_active: bool,
        nym_conf_path: &str,
        lightwalletd_uri: Uri,
        zebrad_uri: Uri,
        max_queue_size: usize,
        max_worker_pool_size: usize,
        idle_worker_pool_size: usize,
        online: Arc<AtomicBool>,
    ) -> Result<Self, ServerError> {
        let (
            request_queue,
            nym_response_queue,
            tcp_ingestor,
            nym_ingestor,
            nym_dispatcher,
            worker_pool,
        ) = match (tcp_active, nym_active) {
            (false, false) => Err(ServerError::ServerConfigError(
                "Cannot start server with no ingestors selected, at least one of nym or tcp must be set to active in conf.".to_string(),
            )),
            (false, true) => {
                let request_queue = Queue::new(max_queue_size);
                let nym_response_queue = Queue::new(max_queue_size);
                let nym_ingestor = Some(
                    NymIngestor::spawn(nym_conf_path, request_queue.tx().clone(), online.clone())
                        .await?,
                );
                let nym_dispatcher = Some(
                    NymDispatcher::spawn(
                        nym_conf_path,
                        nym_response_queue.rx().clone(),
                        nym_response_queue.tx().clone(),
                        online.clone(),
                    )
                    .await?,
                );
                let worker_pool = WorkerPool::spawn(
                    max_worker_pool_size,
                    idle_worker_pool_size,
                    request_queue.rx().clone(),
                    request_queue.tx().clone(),
                    nym_response_queue.tx().clone(),
                    lightwalletd_uri,
                    zebrad_uri,
                    online.clone(),
                )
                .await;
                Ok((
                    request_queue,
                    nym_response_queue,
                    None,
                    nym_ingestor,
                    nym_dispatcher,
                    worker_pool,
                ))
            }
            (true, false) => {
                let request_queue = Queue::new(max_queue_size);
                let nym_response_queue = Queue::new(max_queue_size);
                let tcp_ingestor = Some(
                    TcpIngestor::spawn(
                        tcp_ingestor_listen_addr,
                        request_queue.tx().clone(),
                        online.clone(),
                    )
                    .await?,
                );
                let worker_pool = WorkerPool::spawn(
                    max_worker_pool_size,
                    idle_worker_pool_size,
                    request_queue.rx().clone(),
                    request_queue.tx().clone(),
                    nym_response_queue.tx().clone(),
                    lightwalletd_uri,
                    zebrad_uri,
                    online.clone(),
                )
                .await;
                Ok((
                    request_queue,
                    nym_response_queue,
                    tcp_ingestor,
                    None,
                    None,
                    worker_pool,
                ))
            }
            (true, true) => {
                let request_queue = Queue::new(max_queue_size);
                let nym_response_queue = Queue::new(max_queue_size);
                let tcp_ingestor = Some(
                    TcpIngestor::spawn(
                        tcp_ingestor_listen_addr,
                        request_queue.tx().clone(),
                        online.clone(),
                    )
                    .await?,
                );
                let nym_ingestor = Some(
                    NymIngestor::spawn(nym_conf_path, request_queue.tx().clone(), online.clone())
                        .await?,
                );
                let nym_dispatcher = Some(
                    NymDispatcher::spawn(
                        nym_conf_path,
                        nym_response_queue.rx().clone(),
                        nym_response_queue.tx().clone(),
                        online.clone(),
                    )
                    .await?,
                );
                let worker_pool = WorkerPool::spawn(
                    max_worker_pool_size,
                    idle_worker_pool_size,
                    request_queue.rx().clone(),
                    request_queue.tx().clone(),
                    nym_response_queue.tx().clone(),
                    lightwalletd_uri,
                    zebrad_uri,
                    online.clone(),
                )
                .await;
                Ok((
                    request_queue,
                    nym_response_queue,
                    tcp_ingestor,
                    nym_ingestor,
                    nym_dispatcher,
                    worker_pool,
                ))
            }
        }?;
        let status = ServerStatusType::Spawning(ServerStatus {
            tcp_ingestor_status: IngestorStatus::Inactive,
            nym_ingestor_status: IngestorStatus::Inactive,
            nym_dispatcher_status: DispatcherStatus::Inactive,
            workerpool_status: (
                idle_worker_pool_size,
                max_worker_pool_size,
                vec![WorkerStatusType::Spawning; worker_pool.workers()],
            ),
            request_queue_status: (0, max_queue_size),
            nym_response_queue_status: (0, max_queue_size),
        });
        Ok(Server {
            tcp_ingestor,
            nym_ingestor,
            nym_dispatcher,
            worker_pool,
            request_queue,
            nym_response_queue,
            status,
            online,
        })
    }

    /// Starts the server.
    pub async fn serve(mut self) -> tokio::task::JoinHandle<Result<(), ServerError>> {
        tokio::task::spawn(async move {
            // NOTE: This interval may need to be reduced or removed / moved once scale testing begins.
            let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(50));
            let mut nym_dispatcher_handle = None;
            let mut nym_ingestor_handle = None;
            let mut tcp_ingestor_handle = None;
            let mut worker_handles;
            if let Some(dispatcher) = self.nym_dispatcher {
                nym_dispatcher_handle = Some(dispatcher.serve().await);
            }
            if let Some(ingestor) = self.nym_ingestor {
                nym_ingestor_handle = Some(ingestor.serve().await);
            }
            if let Some(ingestor) = self.tcp_ingestor {
                tcp_ingestor_handle = Some(ingestor.serve().await);
            }
            worker_handles = self.worker_pool.clone().serve().await;
            loop {
                if self.request_queue.queue_length() >= (self.request_queue.max_length() / 2) {
                    match self.worker_pool.push_worker().await {
                        Ok(handle) => {
                            worker_handles.push(handle);
                        }
                        Err(_e) => {
                            eprintln!("WorkerPool at capacity");
                        }
                    }
                } else {
                    let excess_workers: usize = if (self.worker_pool.workers()
                        - self.worker_pool.check_long_standby())
                        < self.worker_pool.idle_size()
                    {
                        self.worker_pool.workers() - self.worker_pool.idle_size()
                    } else {
                        self.worker_pool.check_long_standby()
                    };
                    for i in ((self.worker_pool.workers() - excess_workers)
                        ..self.worker_pool.workers())
                        .rev()
                    {
                        let handle = worker_handles.remove(i);
                        match self.worker_pool.pop_worker(handle).await {
                            Ok(_) => {}
                            Err(e) => {
                                eprintln!("Failed to pop worker from pool: {}", e);
                                // TODO: Handle this error.
                            }
                        }
                    }
                }
                // self.check_statuses();
                // if self.check_for_shutdown().await {
                //     let worker_handle_options: Vec<
                //         Option<tokio::task::JoinHandle<Result<(), WorkerError>>>,
                //     > = worker_handles.into_iter().map(Some).collect();
                //     self.shutdown_components(
                //         tcp_ingestor_handle,
                //         nym_ingestor_handle,
                //         nym_dispatcher_handle,
                //         worker_handle_options,
                //     )
                //     .await;
                //     return Ok(());
                // }
                interval.tick().await;
            }
        })
    }

    /// Checks indexers online status and servers internal status for closure signal.
    pub async fn check_for_shutdown(&self) -> bool {
        if let ServerStatusType::Closing(_) = self.status {
            return true;
        }
        if !self.check_online() {
            return true;
        }
        return false;
    }

    /// Sets the server's components to close gracefully.
    async fn shutdown_components(
        &mut self,
        tcp_ingestor_handle: Option<tokio::task::JoinHandle<Result<(), IngestorError>>>,
        nym_ingestor_handle: Option<tokio::task::JoinHandle<Result<(), IngestorError>>>,
        nym_dispatcher_handle: Option<tokio::task::JoinHandle<Result<(), DispatcherError>>>,
        mut worker_handles: Vec<Option<tokio::task::JoinHandle<Result<(), WorkerError>>>>,
    ) {
        if let Some(ingestor) = self.tcp_ingestor.as_mut() {
            ingestor.shutdown().await;
            if let Some(handle) = tcp_ingestor_handle {
                handle.await.ok();
            }
        }
        if let Some(ingestor) = self.nym_ingestor.as_mut() {
            ingestor.shutdown().await;
            if let Some(handle) = nym_ingestor_handle {
                handle.await.ok();
            }
        }
        self.worker_pool.shutdown(&mut worker_handles).await;
        if let Some(dispatcher) = self.nym_dispatcher.as_mut() {
            dispatcher.shutdown().await;
            if let Some(handle) = nym_dispatcher_handle {
                handle.await.ok();
            }
        }
        self.online
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }

    /// Returns the status of the server and its parts, to be consumed by system printout.
    pub async fn status(&self) -> ServerStatus {
        todo!()
    }

    /// Checks status, handling errors. Returns ServerStatus.
    pub async fn check_statuses(&self) -> ServerStatus {
        todo!()
    }

    /// Check the online status on the indexer.
    fn check_online(&self) -> bool {
        self.online.load(Ordering::SeqCst)
    }
}
