//! Holds the server ingestor (listener) implementations.

use nym_sdk::mixnet::MixnetMessageSender;
use nym_sphinx_anonymous_replies::requests::AnonymousSenderTag;
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tokio::net::TcpListener;

use crate::server::{
    error::{IngestorError, QueueError},
    queue::{QueueReceiver, QueueSender},
    request::ZingoIndexerRequest,
    AtomicStatus, StatusType,
};
use zaino_nym::{client::NymClient, error::NymError};

/// Listens for incoming gRPC requests over HTTP.
pub(crate) struct TcpIngestor {
    /// Tcp Listener.
    ingestor: TcpListener,
    /// Used to send requests to the queue.
    queue: QueueSender<ZingoIndexerRequest>,
    /// Current status of the ingestor.
    status: AtomicStatus,
    /// Represents the Online status of the gRPC server.
    online: Arc<AtomicBool>,
}

impl TcpIngestor {
    /// Creates a Tcp Ingestor.
    pub(crate) async fn spawn(
        listen_addr: SocketAddr,
        queue: QueueSender<ZingoIndexerRequest>,
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
    pub(crate) async fn serve(self) -> tokio::task::JoinHandle<Result<(), IngestorError>> {
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
                                match self.queue.try_send(ZingoIndexerRequest::new_from_grpc(stream)) {
                                    Ok(_) => {
                                        println!("[TEST] Requests in Queue: {}", self.queue.queue_length());
                                    }
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
    pub(crate) async fn check_for_shutdown(&self) -> bool {
        if self.status() >= 4 {
            return true;
        }
        if !self.check_online() {
            return true;
        }
        false
    }

    /// Sets the ingestor to close gracefully.
    pub(crate) async fn _shutdown(&mut self) {
        self.status.store(4)
    }

    /// Returns the ingestor current status usize.
    pub(crate) fn status(&self) -> usize {
        self.status.load()
    }

    /// Returns the ingestor current statustype.
    pub(crate) fn _statustype(&self) -> StatusType {
        StatusType::from(self.status())
    }

    fn check_online(&self) -> bool {
        self.online.load(Ordering::SeqCst)
    }
}

/// Listens for incoming gRPC requests over Nym Mixnet.
pub(crate) struct NymIngestor {
    /// Nym Client
    ingestor: NymClient,
    /// Used to send requests to the queue.
    queue: QueueSender<ZingoIndexerRequest>,
    /// Used to send requests to the queue.
    response_queue: QueueReceiver<(Vec<u8>, AnonymousSenderTag)>,
    /// Used to send requests to the queue.
    response_requeue: QueueSender<(Vec<u8>, AnonymousSenderTag)>,
    /// Current status of the ingestor.
    status: AtomicStatus,
    /// Represents the Online status of the gRPC server.
    online: Arc<AtomicBool>,
}

impl NymIngestor {
    /// Creates a Nym Ingestor
    pub(crate) async fn spawn(
        nym_conf_path: &str,
        queue: QueueSender<ZingoIndexerRequest>,
        response_queue: QueueReceiver<(Vec<u8>, AnonymousSenderTag)>,
        response_requeue: QueueSender<(Vec<u8>, AnonymousSenderTag)>,
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
            response_queue,
            response_requeue,
            online,
            status,
        })
    }

    /// Starts Nym service.
    pub(crate) async fn serve(mut self) -> tokio::task::JoinHandle<Result<(), IngestorError>> {
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
                                // NOTE / TODO: POC server checked for empty messages here (if request.is_empty()). Could be required here...
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
                                    ZingoIndexerRequest::new_from_nym(return_recipient, request_vu8.as_ref())?;
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
                    outgoing = self.response_queue.listen() => {
                        match outgoing {
                            Ok(response) => {
                                println!("[TEST] Dispatcher received response: {:?}", response);
                                // NOTE: This may need to be removed / moved for scale use.
                                if self.check_for_shutdown().await {
                                    self.status.store(5);
                                    return Ok(());
                                }
                                if let Err(nym_e) = self.ingestor
                                        .client
                                        .send_reply(response.1, response.0.clone())
                                        .await.map_err(NymError::from) {
                                    eprintln!("Failed to send response over Nym Mixnet: {}", nym_e);
                                    match self.response_requeue.try_send(response) {
                                        Ok(_) => {
                                            eprintln!("Failed to send response over nym: {}\nResponse requeued, restarting nym dispatcher.", nym_e);
                                            // TODO: Handle error. Restart nym dispatcher.
                                        }
                                        Err(QueueError::QueueFull(_request)) => {
                                            eprintln!("Failed to send response over nym: {}\nAnd failed to requeue response due to full response queue.\nFatal error! Restarting nym dispatcher.", nym_e);
                                            // TODO: Handle queue full error here (start up second dispatcher?). Restart nym dispatcher
                                        }
                                        Err(_e) => {
                                            eprintln!("Failed to send response over nym: {}\nAnd failed to requeue response due to the queue being closed.\nFatal error! Nym dispatcher shutting down..", nym_e);
                                            // TODO: Handle queue closed error here. (return correct error type?)
                                            self.status.store(6);
                                            return Ok(()); //Return Err!
                                        }
                                    }
                                }
                            }
                            Err(_e) => {
                                eprintln!("Response queue closed, nym dispatcher shutting down.");
                                //TODO: Handle this error here (return correct error type?)
                                self.status.store(6);
                                return Ok(()); // Return Err!
                            }
                        }
                    }
                }
            }
        })
    }

    /// Checks indexers online status and ingestors internal status for closure signal.
    pub(crate) async fn check_for_shutdown(&self) -> bool {
        if self.status() >= 4 {
            return true;
        }
        if !self.check_online() {
            return true;
        }
        false
    }

    /// Sets the ingestor to close gracefully.
    pub(crate) async fn _shutdown(&mut self) {
        self.status.store(4)
    }

    /// Returns the ingestor current status usize.
    pub(crate) fn status(&self) -> usize {
        self.status.load()
    }

    /// Returns the ingestor current statustype.
    pub(crate) fn _statustype(&self) -> StatusType {
        StatusType::from(self.status())
    }

    fn check_online(&self) -> bool {
        self.online.load(Ordering::SeqCst)
    }
}
