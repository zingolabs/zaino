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

use std::future::Future;
use std::sync::{Arc, Mutex};

use tokio::sync::watch;
use zaino_component::{
    CancellationToken, ComponentName, ComponentStatus, Health, Lifecycle, Managed, ReadySignal,
    StatusSource, StatusWatch, Task, TaskName,
};

/// Drives index-building: sources blocks and writes the index.
///
/// `run` starts syncing, fires `caught_up` the first time it reaches the tip,
/// and follows the chain until `cancel`. `Ok(())` is a clean stop; `Err` is a
/// source/write failure — which makes the component `Critical`.
pub trait SyncDriver: Send + Sync + 'static {
    /// Why syncing could not continue.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Build the index until cancelled, firing `caught_up` once at the tip.
    fn run(
        self: Arc<Self>,
        cancel: CancellationToken,
        caught_up: ReadySignal,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

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
        *self.status.borrow()
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
            });
        });

        let driver = Arc::clone(&self.driver);
        let syncing_status = self.status.clone();
        let done_status = self.status.clone();
        let task = Task::spawn(TaskName(self.name.0), move |cancel| async move {
            // Now actively building the index.
            syncing_status.send_modify(|s| {
                s.lifecycle = Lifecycle::Syncing;
                s.health = Health::Healthy;
            });
            match driver.run(cancel, caught_up).await {
                Ok(()) => done_status.send_modify(|s| {
                    s.lifecycle = Lifecycle::Offline;
                    s.health = Health::Offline;
                }),
                Err(_) => done_status.send_modify(|s| s.health = Health::Critical),
            }
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
        });
        Ok(())
    }
}
