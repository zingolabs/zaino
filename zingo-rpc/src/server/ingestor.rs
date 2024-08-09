//! Holds the server ingestor (listener) implementations.

use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tokio::net::TcpListener;

use crate::{
    nym::{client::NymClient, error::NymError},
    server::{
        error::{IngestorError, QueueError},
        queue::QueueSender,
        request::ZingoProxyRequest,
        AtomicStatus, StatusType,
    },
};

/// Listens for incoming gRPC requests over HTTP.
pub struct TcpIngestor {
    /// Tcp Listener.
    ingestor: TcpListener,
    /// Used to send requests to the queue.
    queue: QueueSender<ZingoProxyRequest>,
    /// Current status of the ingestor.
    status: AtomicStatus,
    /// Represents the Online status of the gRPC server.
    online: Arc<AtomicBool>,
}

impl TcpIngestor {
    /// Creates a Tcp Ingestor.
    pub async fn spawn(
        listen_addr: SocketAddr,
        queue: QueueSender<ZingoProxyRequest>,
        status: AtomicStatus,
        online: Arc<AtomicBool>,
    ) -> Result<Self, IngestorError> {
        status.store(0);
        let listener = TcpListener::bind(listen_addr).await?;
        println!("TcpIngestor listening at: {}.", listen_addr);
        Ok(TcpIngestor {
            ingestor: listener,
            queue,
            online,
            status,
        })
    }

    /// Starts Tcp service.
    pub async fn serve(self) -> tokio::task::JoinHandle<Result<(), IngestorError>> {
        tokio::task::spawn(async move {
            // NOTE: This interval may need to be changed or removed / moved once scale testing begins.
            let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(50));
            // TODO Check blockcache sync status and wait on server / node if on hold.
            self.status.store(1);
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        if self.check_for_shutdown().await {
                            self.status.store(5);
                            return Ok(());
                        }
                    }
                    incoming = self.ingestor.accept() => {
                        // NOTE: This may need to be removed / moved for scale use.
                        if self.check_for_shutdown().await {
                            self.status.store(5);
                            return Ok(());
                        }
                        match incoming {
                            Ok((stream, _)) => {
                                match self.queue.try_send(ZingoProxyRequest::new_from_grpc(stream)) {
                                    Ok(_) => {}
                                    Err(QueueError::QueueFull(_request)) => {
                                        eprintln!("Queue Full.");
                                        // TODO: Return queue full tonic status over tcpstream and close (that TcpStream..).
                                    }
                                    Err(e) => {
                                        eprintln!("Queue Closed. Failed to send request to queue: {}", e);
                                        // TODO: Handle queue closed error here.
                                    }
                                }
                            }
                            Err(e) => {
                                eprintln!("Failed to accept connection with client: {}", e);
                                // TODO: Handle failed connection errors here (count errors and restart ingestor / proxy or initiate shotdown?)
                            }
                        }
                    }
                }
            }
        })
    }

    /// Checks indexers online status and ingestors internal status for closure signal.
    pub async fn check_for_shutdown(&self) -> bool {
        if self.status() >= 4 {
            return true;
        }
        if !self.check_online() {
            return true;
        }
        false
    }

    /// Sets the ingestor to close gracefully.
    pub async fn shutdown(&mut self) {
        self.status.store(4)
    }

    /// Returns the ingestor current status usize.
    pub fn status(&self) -> usize {
        self.status.load()
    }

    /// Returns the ingestor current statustype.
    pub fn statustype(&self) -> StatusType {
        StatusType::from(self.status())
    }

    fn check_online(&self) -> bool {
        self.online.load(Ordering::SeqCst)
    }
}

/// Listens for incoming gRPC requests over Nym Mixnet.
pub struct NymIngestor {
    /// Nym Client
    ingestor: NymClient,
    /// Used to send requests to the queue.
    queue: QueueSender<ZingoProxyRequest>,
    /// Current status of the ingestor.
    status: AtomicStatus,
    /// Represents the Online status of the gRPC server.
    online: Arc<AtomicBool>,
}

impl NymIngestor {
    /// Creates a Nym Ingestor
    pub async fn spawn(
        nym_conf_path: &str,
        queue: QueueSender<ZingoProxyRequest>,
        status: AtomicStatus,
        online: Arc<AtomicBool>,
    ) -> Result<Self, IngestorError> {
        status.store(0);
        // TODO: HANDLE THESE ERRORS TO SMOOTH MIXNET CLIENT SPAWN PROCESS!
        let listener = NymClient::spawn(&format!("{}/ingestor", nym_conf_path)).await?;
        println!("NymIngestor listening at: {}.", listener.addr);
        Ok(NymIngestor {
            ingestor: listener,
            queue,
            online,
            status,
        })
    }

    /// Starts Nym service.
    pub async fn serve(mut self) -> tokio::task::JoinHandle<Result<(), IngestorError>> {
        tokio::task::spawn(async move {
            // NOTE: This interval may need to be reduced or removed / moved once scale testing begins.
            let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(50));
            // TODO Check blockcache sync status and wait on server / node if on hold.
            self.status.store(1);
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        if self.check_for_shutdown().await {
                            self.status.store(5);
                            return Ok(())
                        }
                    }
                    incoming = self.ingestor.client.wait_for_messages() => {
                        // NOTE: This may need to be removed /moved for scale use.
                        if self.check_for_shutdown().await {
                            self.status.store(5);
                            return Ok(())
                        }
                        match incoming {
                            Some(request) => {
                                // NOTE / TODO: POC server checked for empty emssages here (if request.is_empty()). Could be required here...
                                // TODO: Handle EmptyMessageError here.
                                let request_vu8 = request
                                    .first()
                                    .map(|r| r.message.clone())
                                    .ok_or_else(|| IngestorError::NymError(NymError::EmptyMessageError))?;
                                // TODO: Handle EmptyRecipientTagError here.
                                let return_recipient = request[0]
                                    .sender_tag
                                    .ok_or_else(|| IngestorError::NymError(NymError::EmptyRecipientTagError))?;
                                // TODO: Handle RequestError here.
                                let zingo_proxy_request =
                                    ZingoProxyRequest::new_from_nym(return_recipient, request_vu8.as_ref())?;
                                match self.queue.try_send(zingo_proxy_request) {
                                    Ok(_) => {}
                                    Err(QueueError::QueueFull(_request)) => {
                                        eprintln!("Queue Full.");
                                        // TODO: Return queue full tonic status over mixnet.
                                    }
                                    Err(e) => {
                                        eprintln!("Queue Closed. Failed to send request to queue: {}", e);
                                        // TODO: Handle queue closed error here.
                                    }
                                }
                            }
                            None => {
                                eprintln!("Failed to receive message from Nym network.");
                                // TODO: Error in nym client, handle error here (restart ingestor?).
                            }
                        }
                    }
                }
            }
        })
    }

    /// Checks indexers online status and ingestors internal status for closure signal.
    pub async fn check_for_shutdown(&self) -> bool {
        if self.status() >= 4 {
            return true;
        }
        if !self.check_online() {
            return true;
        }
        false
    }

    /// Sets the ingestor to close gracefully.
    pub async fn shutdown(&mut self) {
        self.status.store(4)
    }

    /// Returns the ingestor current status usize.
    pub fn status(&self) -> usize {
        self.status.load()
    }

    /// Returns the ingestor current statustype.
    pub fn statustype(&self) -> StatusType {
        StatusType::from(self.status())
    }

    fn check_online(&self) -> bool {
        self.online.load(Ordering::SeqCst)
    }
}
