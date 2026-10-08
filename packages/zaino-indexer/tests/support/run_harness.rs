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

/// The bound on every wait in the harness: a run that never reaches its
/// condition fails the test cleanly rather than hanging CI until the outer
/// harness kills it.
const DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);

impl<E: std::fmt::Debug> TestRun<E> {
    /// Resolve once the loop reports `Ready`, or fail if it does not within the
    /// [`DEADLINE`].
    pub async fn await_ready(&mut self) {
        tokio::time::timeout(DEADLINE, async {
            while !*self.ready.borrow_and_update() {
                self.ready
                    .changed()
                    .await
                    .expect("the reporter lives until ready");
            }
        })
        .await
        .expect("the loop reached Ready within the deadline");
    }

    /// Cancel the loop and await a clean stop, or fail if it does not stop within
    /// the [`DEADLINE`].
    pub async fn stop(self) -> Result<(), E> {
        self.cancel.cancel();
        tokio::time::timeout(DEADLINE, self.handle)
            .await
            .expect("the run task stops within the deadline")
            .expect("the run task joins")
    }
}
