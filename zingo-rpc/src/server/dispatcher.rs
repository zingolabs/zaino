//! Holds the server dispatcher (replyer) implementations.

use nym_sdk::mixnet::MixnetMessageSender;
use nym_sphinx_anonymous_replies::requests::AnonymousSenderTag;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use crate::{
    nym::{client::NymClient, error::NymError},
    server::{
        error::{DispatcherError, QueueError},
        queue::{QueueReceiver, QueueSender},
        AtomicStatus, StatusType,
    },
};

/// Sends gRPC responses over Nym Mixnet.
pub struct NymDispatcher {
    /// Nym Client
    dispatcher: NymClient,
    /// Used to send requests to the queue.
    response_queue: QueueReceiver<(Vec<u8>, AnonymousSenderTag)>,
    /// Used to send requests to the queue.
    response_requeue: QueueSender<(Vec<u8>, AnonymousSenderTag)>,
    /// Current status of the ingestor.
    status: AtomicStatus,
    /// Represents the Online status of the gRPC server.
    online: Arc<AtomicBool>,
}

impl NymDispatcher {
    /// Creates a Nym Ingestor
    pub async fn spawn(
        nym_conf_path: &str,
        response_queue: QueueReceiver<(Vec<u8>, AnonymousSenderTag)>,
        response_requeue: QueueSender<(Vec<u8>, AnonymousSenderTag)>,
        status: AtomicStatus,
        online: Arc<AtomicBool>,
    ) -> Result<Self, DispatcherError> {
        status.store(0);
        let client = NymClient::spawn(&format!("{}/dispatcher", nym_conf_path)).await?;
        Ok(NymDispatcher {
            dispatcher: client,
            response_queue,
            response_requeue,
            online,
            status,
        })
    }

    /// Starts Nym service.
    pub async fn serve(self) -> tokio::task::JoinHandle<Result<(), DispatcherError>> {
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
                    incoming = self.response_queue.listen() => {
                        match incoming {
                            Ok(response) => {
                                // NOTE: This may need to be removed / moved for scale use.
                                if self.check_for_shutdown().await {
                                    self.status.store(5);
                                    return Ok(());
                                }
                                if let Err(nym_e) = self.dispatcher
                                        .client
                                        .send_reply(response.1, response.0.clone())
                                        .await.map_err(NymError::from) {
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
    pub async fn check_for_shutdown(&self) -> bool {
        if self.status() >= 4 {
            return true;
        }
        if !self.check_online() {
            return true;
        }
        false
    }

    /// Sets the dispatcher to close gracefully.
    pub async fn shutdown(&mut self) {
        self.status.store(4)
    }

    /// Returns the dispatchers current status usize.
    pub fn status(&self) -> usize {
        self.status.load()
    }

    /// Returns the dispatchers current statustype.
    pub fn statustype(&self) -> StatusType {
        StatusType::from(self.status())
    }

    fn check_online(&self) -> bool {
        self.online.load(Ordering::SeqCst)
    }
}
