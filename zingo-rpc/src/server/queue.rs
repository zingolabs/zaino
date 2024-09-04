//! Zingo-Indexer queue implementation.

use crossbeam_channel::{bounded, Receiver, Sender};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use crate::server::error::QueueError;

/// Queue with max length.
#[derive(Debug, Clone)]
pub struct Queue<T> {
    /// Max number of messages allowed in the queue.
    max_length: usize,
    /// Used to track current messages in the queue.
    queue_status: Arc<AtomicUsize>,
    /// Queue sender.
    queue_tx: QueueSender<T>,
    /// Queue receiver.
    queue_rx: QueueReceiver<T>,
}

impl<T> Queue<T> {
    /// Creates a new queue with a maximum size.
    pub fn new(max_length: usize, queue_status: Arc<AtomicUsize>) -> Self {
        let (queue_tx, queue_rx) = bounded(max_length);
        queue_status.store(0, Ordering::SeqCst);
        Queue {
            max_length,
            queue_status: queue_status.clone(),
            queue_tx: QueueSender {
                inner: queue_tx,
                queue_status: queue_status.clone(),
            },
            queue_rx: QueueReceiver {
                inner: queue_rx,
                queue_status,
            },
        }
    }

    /// Returns a queue transmitter.
    pub fn tx(&self) -> QueueSender<T> {
        self.queue_tx.clone()
    }

    /// Returns a queue receiver.
    pub fn rx(&self) -> QueueReceiver<T> {
        self.queue_rx.clone()
    }

    /// Returns the max length of the queue.
    pub fn max_length(&self) -> usize {
        self.max_length
    }

    /// Returns the current length of the queue.
    pub fn queue_length(&self) -> usize {
        self.queue_status.load(Ordering::SeqCst)
    }
}

/// Sends messages to a queue.
#[derive(Debug)]
pub struct QueueSender<T> {
    /// Crossbeam_Channel Sender.
    inner: Sender<T>,
    /// Used to track current messages in the queue.
    queue_status: Arc<AtomicUsize>,
}

impl<T> Clone for QueueSender<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            queue_status: Arc::clone(&self.queue_status),
        }
    }
}

impl<T> QueueSender<T> {
    /// Tries to add a request to the queue, updating the queue size.
    pub fn try_send(&self, message: T) -> Result<(), QueueError<T>> {
        match self.inner.try_send(message) {
            Ok(_) => {
                self.queue_status.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            Err(crossbeam_channel::TrySendError::Full(t)) => Err(QueueError::QueueFull(t)),
            Err(crossbeam_channel::TrySendError::Disconnected(_)) => Err(QueueError::QueueClosed),
        }
    }

    /// Returns the current length of the queue.
    pub fn queue_length(&self) -> usize {
        self.queue_status.load(Ordering::SeqCst)
    }
}

/// Receives messages from a queue.
#[derive(Debug)]
pub struct QueueReceiver<T> {
    /// Crossbeam_Channel Receiver.
    inner: Receiver<T>,
    /// Used to track current messages in the queue.
    queue_status: Arc<AtomicUsize>,
}

impl<T> Clone for QueueReceiver<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            queue_status: Arc::clone(&self.queue_status),
        }
    }
}

impl<T> QueueReceiver<T> {
    /// Try to receive a request from the queue, updatig queue size.
    pub fn try_recv(&self) -> Result<T, QueueError<T>> {
        match self.inner.try_recv() {
            Ok(message) => {
                self.queue_status.fetch_sub(1, Ordering::SeqCst);
                Ok(message)
            }
            Err(crossbeam_channel::TryRecvError::Empty) => Err(QueueError::QueueEmpty),
            Err(crossbeam_channel::TryRecvError::Disconnected) => Err(QueueError::QueueClosed),
        }
    }

    /// Listens indefinately for an incoming message on the queue. Returns message if received or error if queue is closed.
    pub async fn listen(&self) -> Result<T, QueueError<T>> {
        // NOTE: This interval may need to be reduced or removed / moved once scale testing begins.
        let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(50));
        loop {
            match self.try_recv() {
                Ok(message) => {
                    return Ok(message);
                }
                Err(QueueError::QueueEmpty) => {
                    interval.tick().await;
                    continue;
                }
                Err(e) => {
                    return Err(e);
                }
            }
        }
    }

    /// Returns the current length of the queue.
    pub fn queue_length(&self) -> usize {
        self.queue_status.load(Ordering::SeqCst)
    }
}
