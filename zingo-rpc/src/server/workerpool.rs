//! Holds the server workerpool implementation.

use std::sync::{atomic::AtomicBool, Arc};

use crate::server::worker::Worker;

/// Dynamically sized pool of workers.
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
    // /// Creates a new worker pool with idle_workers in it.
    // pub async fn spawn(max_size: usize, idle_size: usize, online: Arc<AtomicBool>) -> Self {
    //     let workers: Vec<Worker> = Vec::with_capacity(max_size);
    //     for i in 0..idle_size {
    //         workers.push(
    //             Worker::spawn(
    //                 i,
    //                 queue,
    //                 _requeue,
    //                 nym_response_queue,
    //                 lightwalletd_uri,
    //                 zebrad_uri,
    //                 online.clone(),
    //             )
    //             .await,
    //         );
    //     }

    //     WorkerPool {
    //         max_size,
    //         idle_size,
    //         workers,
    //         online,
    //     }
    // }

    // Sets workers in the worker pool to start servicing the queue.
    // pub fn serve(&self) -> Vec<tokio::task::JoinHandle<Result<(), WorkerError>>> {}

    // Adds a worker to the worker pool, returns error if the pool is already at max size.
    // pub fn add_worker(&self) -> tokio::task::JoinHandle<Result<(), WorkerError>> {}

    // Checks workers on standby, closes workers that have been on standby for longer than 30s(may need to change).
    // pub fn check_workers(&self) {}
}
