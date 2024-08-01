//! Holds the ingestor (listener) implementations.

use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tokio::{net::TcpListener, sync::mpsc};

use crate::{
    nym::{client::NymClient, error::NymError},
    queue::{error::IngestorError, request::ZingoProxyRequest},
};

/// Status of the worker.
///
/// TODO: Add duration to each variant.
#[derive(Debug, Clone)]
pub enum IngestorStatus {
    /// On hold, due to blockcache / node error.
    Inactive,
    /// Processing requests from the queue.
    Listening,
}

/// Configuration data for gRPC server.
pub struct TcpIngestor {
    /// Tcp Listener.
    ingestor: TcpListener,
    /// Used to send requests to the queue.
    queue: mpsc::Sender<ZingoProxyRequest>,
    /// Represents the Online status of the gRPC server.
    online: Arc<AtomicBool>,
    /// Current status of the ingestor.
    status: IngestorStatus,
}

impl TcpIngestor {
    /// Creates a Tcp Ingestor.
    pub async fn spawn(
        listen_addr: SocketAddr,
        queue: mpsc::Sender<ZingoProxyRequest>,
        online: Arc<AtomicBool>,
    ) -> Result<Self, IngestorError> {
        let listener = TcpListener::bind(listen_addr).await?;
        Ok(TcpIngestor {
            ingestor: listener,
            queue,
            online,
            status: IngestorStatus::Inactive,
        })
    }

    /// Starts Tcp service.
    pub fn serve(mut self) -> tokio::task::JoinHandle<Result<(), IngestorError>> {
        tokio::task::spawn(async move {
            // NOTE: This interval may need to be reduced or removed / moved once scale testing begins.
            let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(50));
            // TODO Check self.status and wait on server / node if on hold.
            self.status = IngestorStatus::Listening;
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        if !self.check_online() {
                            println!("Tcp ingestor shutting down.");
                            return Ok(());
                        }
                    }
                    incoming = self.ingestor.accept() => {
                        match incoming {
                            Ok((stream, _)) => {
                                if !self.check_online() {
                                    println!("Tcp ingestor shutting down.");
                                    return Ok(());
                                }
                                if let Err(e) = self.queue.send(ZingoProxyRequest::new_from_grpc(stream)).await {
                                    // TODO:: Return queue full tonic status over tcpstream and close (that TcpStream..).
                                    eprintln!("Failed to send connection: {}", e);
                                }
                            }
                            Err(e) => {
                                // TODO: Handle error here (count errors and restart ingestor / proxy or initiate shotdown?)
                                eprintln!("Failed to accept connection with client: {}", e);
                                if !self.check_online() {
                                    println!("Tcp ingestor shutting down.");
                                    return Ok(());
                                }
                                continue;
                            }
                        }
                    }
                }
            }
        })
    }

    fn check_online(&self) -> bool {
        self.online.load(Ordering::SeqCst)
    }
}

/// Wrapper struct for a Nym client.
pub struct NymIngestor {
    /// Nym Client
    ingestor: NymClient,
    /// Used to send requests to the queue.
    queue: mpsc::Sender<ZingoProxyRequest>,
    /// Represents the Online status of the gRPC server.
    online: Arc<AtomicBool>,
    /// Current status of the ingestor.
    status: IngestorStatus,
}

impl NymIngestor {
    /// Creates a Nym Ingestor
    pub async fn spawn(
        nym_conf_path: &str,
        queue: mpsc::Sender<ZingoProxyRequest>,
        online: Arc<AtomicBool>,
    ) -> Result<Self, IngestorError> {
        let listener = NymClient::spawn(nym_conf_path).await?;
        Ok(NymIngestor {
            ingestor: listener,
            queue,
            online,
            status: IngestorStatus::Inactive,
        })
    }

    /// Starts Nym service.
    pub async fn serve(mut self) -> tokio::task::JoinHandle<Result<(), IngestorError>> {
        tokio::task::spawn(async move {
            // NOTE: This interval may need to be reduced or removed / moved once scale testing begins.
            let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(50));
            // TODO Check self.status and wait on server / node if on hold.
            self.status = IngestorStatus::Listening;

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        if !self.check_online() {
                            println!("Nym ingestor shutting down.");
                            return Ok(());
                        }
                    }
                    incoming = self.ingestor.client.wait_for_messages() => {
                        match incoming {
                            Some(request) => {
                                if !self.check_online() {
                                    println!("Nym ingestor shutting down.");
                                    return Ok(());
                                }
                                // NOTE / TODO: POC server checked for empty emssages here (if request.is_empty()..). Could be required here.
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
                                if let Err(e) = self.queue.send(zingo_proxy_request).await {
                                    // TODO: Return queue full tonic status over nym mixnet.
                                    eprintln!("Failed to send connection: {}", e);
                                }
                            }
                            None => {
                                eprintln!("Failed to receive message from Nym network.");
                                if !self.online.load(Ordering::SeqCst) {
                                    println!("Nym ingestor shutting down.");
                                    return Ok(());
                                }
                            }
                        }
                    }
                }
            }
        })
    }

    fn check_online(&self) -> bool {
        self.online.load(Ordering::SeqCst)
    }
}
