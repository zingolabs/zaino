//! The indexer — the *writer* — as a supervised component.
//!
//! Building the index and answering queries from it are two components (the
//! writer / sync engine vs the store / reader). This is the writer: it sources
//! blocks and builds the index, depending on the validator. Its lifecycle is
//! `Spawning → Syncing → Ready(caught-up)`, distinct from a server's
//! `Spawning → Ready(bound)`: the `Syncing` phase is real and load-bearing —
//! that is what keeps the runtime in `Booting` (under full mode) while the
//! initial sync runs, and what the readiness gate (`sync_gated`) keys off.
//!
//! It is **owned** (`Managed`): the runtime spawns / restarts / stops it, unlike
//! the observed validator. The concrete sync engine (e.g. ChainView's sync)
//! implements [`SyncDriver`]; this component is the runtime slot it plugs into.

use std::sync::{Arc, Mutex};

use tokio::sync::watch;
use zaino_async::{Task, TaskName};
use zaino_component::{
    ComponentName, ComponentStatus, Health, Lifecycle, Managed, ReadySignal, StatusSource,
    StatusWatch, SyncDriver,
};

/// A [`SyncDriver`] presented to the runtime as an owned component.
pub struct IndexerComponent<D> {
    name: ComponentName,
    driver: Arc<D>,
    status: watch::Sender<ComponentStatus>,
    task: Arc<Mutex<Option<Task>>>,
}

impl<D> Clone for IndexerComponent<D> {
    fn clone(&self) -> Self {
        Self {
            name: self.name,
            driver: Arc::clone(&self.driver),
            status: self.status.clone(),
            task: Arc::clone(&self.task),
        }
    }
}

impl<D> IndexerComponent<D> {
    /// An indexer named `name` driven by `driver`, initially `Offline`.
    pub fn new(name: ComponentName, driver: D) -> Self {
        let (status, _) = watch::channel(ComponentStatus::new(
            name,
            Lifecycle::Offline,
            Health::Offline,
        ));
        Self {
            name,
            driver: Arc::new(driver),
            status,
            task: Arc::new(Mutex::new(None)),
        }
    }
}

impl<D: Send + Sync + 'static> StatusSource for IndexerComponent<D> {
    fn status(&self) -> ComponentStatus {
        self.status.borrow().clone()
    }
}

impl<D: Send + Sync + 'static> StatusWatch for IndexerComponent<D> {
    fn subscribe(&self) -> watch::Receiver<ComponentStatus> {
        self.status.subscribe()
    }
}

impl<D: SyncDriver> Managed for IndexerComponent<D> {
    // Starting the driver cannot fail synchronously; a source/write failure
    // surfaces as the run task's `Err`, flipping health `Critical`.
    type Error = std::convert::Infallible;

    async fn spawn(&self) -> Result<(), Self::Error> {
        self.status
            .send_modify(|s| s.lifecycle = Lifecycle::Spawning);

        // Report `Ready` when the driver first reaches the tip; until then it is
        // `Syncing`.
        let caught_up_status = self.status.clone();
        let caught_up = ReadySignal::new(move || {
            caught_up_status.send_modify(|s| {
                s.lifecycle = Lifecycle::Ready;
                s.health = Health::Healthy;
                s.reason = None;
            });
        });

        let driver = Arc::clone(&self.driver);
        let run_status = self.status.clone();
        let name = self.name;
        let task = Task::spawn(TaskName(self.name.0), move |cancel| async move {
            // Now actively building the index.
            run_status.send_modify(|s| {
                s.lifecycle = Lifecycle::Syncing;
                s.health = Health::Healthy;
                s.reason = None;
            });
            // The run/serve boundary — catch a panic, log any failure, reconcile
            // the terminal status — is one shared seam (see `crate::run`), so this
            // component neither re-implements the match nor forgets to log.
            crate::run::run_and_reconcile(
                name,
                &run_status,
                "run loop",
                driver.run(cancel, caught_up),
            )
            .await;
        });
        *self.task.lock().expect("indexer task mutex poisoned") = Some(task);
        Ok(())
    }

    async fn restart(&self) -> Result<(), Self::Error> {
        self.stop().await?;
        self.spawn().await
    }

    async fn stop(&self) -> Result<(), Self::Error> {
        self.status
            .send_modify(|s| s.lifecycle = Lifecycle::Closing);
        let task = self
            .task
            .lock()
            .expect("indexer task mutex poisoned")
            .take();
        if let Some(task) = task {
            task.cancel();
            let _ = task.join().await;
        }
        self.status.send_modify(|s| {
            s.lifecycle = Lifecycle::Offline;
            s.health = Health::Offline;
            s.reason = None;
        });
        Ok(())
    }
}
