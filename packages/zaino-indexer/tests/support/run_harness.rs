//! A minimal direct-drive harness for a [`RunLoop`] in integration tests.
//!
//! Stands in for the runtime's `RunComponent` without depending on
//! `zaino-runtime`: it spawns the loop, exposes a readiness signal, and stops it
//! cleanly by cancellation. Shared across test files via `#[path]`, so not every
//! item is used in every binary.
#![allow(dead_code)]

use std::sync::Arc;

use tokio::sync::watch;
use zaino_component::{CancellationToken, RunLoop, RunReport, RunReporter};

/// A spawned [`RunLoop`], with a readiness signal and a clean-stop handle.
pub struct TestRun<E> {
    cancel: CancellationToken,
    ready: watch::Receiver<bool>,
    handle: tokio::task::JoinHandle<Result<(), E>>,
}

/// Spawn `run_loop`, returning a handle that observes readiness and stops it.
pub fn drive<L: RunLoop>(run_loop: L) -> TestRun<L::Error> {
    let cancel = CancellationToken::new();
    let (ready_tx, ready_rx) = watch::channel(false);
    let reporter = RunReporter::new(move |report| {
        if matches!(report, RunReport::Ready) {
            let _ = ready_tx.send(true);
        }
    });
    let run_loop = Arc::new(run_loop);
    let handle = tokio::spawn({
        let cancel = cancel.clone();
        async move { run_loop.run(cancel, reporter).await }
    });
    TestRun {
        cancel,
        ready: ready_rx,
        handle,
    }
}

impl<E: std::fmt::Debug> TestRun<E> {
    /// Resolve once the loop reports `Ready`.
    pub async fn await_ready(&mut self) {
        while !*self.ready.borrow_and_update() {
            self.ready
                .changed()
                .await
                .expect("the reporter lives until ready");
        }
    }

    /// Cancel the loop and await a clean stop.
    pub async fn stop(self) -> Result<(), E> {
        self.cancel.cancel();
        self.handle.await.expect("the run task joins")
    }
}
