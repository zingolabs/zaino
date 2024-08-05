//! Holds the server dispatcher (replyer) implementations.

use nym_sdk::mixnet::MixnetMessageSender;
use nym_sphinx_anonymous_replies::requests::AnonymousSenderTag;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::sync::mpsc;

use crate::{
    nym::{client::NymClient, error::NymError},
    server::error::{DispatcherError, QueueError},
    server::queue::{QueueReceiver, QueueSender},
};

/// Status of the worker.
#[derive(Debug, Clone)]
pub enum DispatcherStatus {
    /// On hold, due to blockcache / node error.
    Inactive,
    /// Listening for new requests.
    Listening,
}

/// Sends gRPC responses over Nym Mixnet.
pub struct NymDispatcher {
    /// Nym Client
    dispatcher: NymClient,
    /// Used to send requests to the queue.
    response_queue: QueueReceiver<(Vec<u8>, AnonymousSenderTag)>,
    /// Used to send requests to the queue.
    response_requeue: QueueSender<(Vec<u8>, AnonymousSenderTag)>,
    /// Represents the Online status of the gRPC server.
    online: Arc<AtomicBool>,
    /// Current status of the ingestor.
    status: DispatcherStatus,
}

impl NymDispatcher {
    /// Creates a Nym Ingestor
    pub async fn spawn(
        nym_conf_path: &str,
        response_queue: QueueReceiver<(Vec<u8>, AnonymousSenderTag)>,
        response_requeue: QueueSender<(Vec<u8>, AnonymousSenderTag)>,
        online: Arc<AtomicBool>,
    ) -> Result<Self, DispatcherError> {
        let client = NymClient::spawn(&format!("{}/dispatcher", nym_conf_path)).await?;
        Ok(NymDispatcher {
            dispatcher: client,
            response_queue,
            response_requeue,
            online,
            status: DispatcherStatus::Inactive,
        })
    }

    /// Starts Nym service.
    pub async fn serve(mut self) -> tokio::task::JoinHandle<Result<(), DispatcherError>> {
        tokio::task::spawn(async move {
            // NOTE: This interval may need to be changed or removed / moved once scale testing begins.
            let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(50));
            // TODO Check self.status and wait on server / node if on hold.
            self.status = DispatcherStatus::Listening;
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        if !self.check_online() {
                            println!("Nym dispatcher shutting down.");
                            return Ok(());
                        }
                    }
                    incoming = self.response_queue.listen() => {
                        match incoming {
                            Ok(response) => {
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
                                            return Ok(()); //Return Err!
                                        }
                                    }
                                }
                            }
                            Err(_e) => {
                                //TODO: Handle this error here (return correct error type?)
                                eprintln!("Response queue closed, nym dispatcher shutting down.");
                                return Ok(()); // Return Err!
                            }
                        }
                    }
                }
            }
        })
    }

    /// Ends the dispatcher.
    pub async fn shutdown(self) {
        todo!()
    }

    /// Returns the dispatchers current status.
    pub fn status(&self) -> DispatcherStatus {
        self.status.clone()
    }

    fn check_online(&self) -> bool {
        self.online.load(Ordering::SeqCst)
    }
}
