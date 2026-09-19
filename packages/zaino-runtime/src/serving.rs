//! A server as a supervised component.
//!
//! [`Serve`] is the seam between the runtime's supervision and a concrete
//! transport server (tonic / jsonrpsee): a long-running workload that binds and
//! runs until cancelled. [`ServeComponent`] drives a `Serve` as a
//! [`zaino_component`] component — it reports a [`ComponentStatus`], publishes it
//! on a `watch`, and is [`Managed`]. Lifecycle and health are owned here and
//! observed by the Orchestra, replacing the servers' hand-rolled
//! `NamedAtomicStatus`.
//!
//! Health is driven by the serve task's outcome, not injected: a clean shutdown
//! (the token fired, `Ok`) settles the component `Offline`; an early `Err` (a
//! bind failure, a serve loop that died) flips it `Critical`, which the
//! Orchestra escalates.

use std::sync::{Arc, Mutex};

use tokio::sync::watch;
use zaino_component::{
    ComponentName, ComponentStatus, Health, Lifecycle, Managed, ReadySignal, Serve, StatusSource,
    StatusWatch, Task, TaskName,
};

/// A [`Serve`] server `A`, presented to the runtime as a component.
pub struct ServeComponent<A> {
    name: ComponentName,
    server: Arc<A>,
    status: watch::Sender<ComponentStatus>,
    task: Arc<Mutex<Option<Task>>>,
}

impl<A> Clone for ServeComponent<A> {
    fn clone(&self) -> Self {
        Self {
            name: self.name,
            server: Arc::clone(&self.server),
            status: self.status.clone(),
            task: Arc::clone(&self.task),
        }
    }
}

impl<A> ServeComponent<A> {
    /// A component named `name` supervising `server`, initially `Offline`.
    pub fn new(name: ComponentName, server: A) -> Self {
        let (status, _) = watch::channel(ComponentStatus::new(
            name,
            Lifecycle::Offline,
            Health::Offline,
        ));
        Self {
            name,
            server: Arc::new(server),
            status,
            task: Arc::new(Mutex::new(None)),
        }
    }
}

impl<A: Send + Sync + 'static> StatusSource for ServeComponent<A> {
    fn status(&self) -> ComponentStatus {
        *self.status.borrow()
    }
}

impl<A: Send + Sync + 'static> StatusWatch for ServeComponent<A> {
    fn subscribe(&self) -> watch::Receiver<ComponentStatus> {
        self.status.subscribe()
    }
}

impl<A: Serve> Managed for ServeComponent<A> {
    // Starting a component never fails synchronously: a bind failure surfaces as
    // the serve task's early `Err`, which flips health `Critical` for the
    // Orchestra to escalate — the same reactive path as a mid-run failure.
    type Error = std::convert::Infallible;

    async fn spawn(&self) -> Result<(), Self::Error> {
        self.status
            .send_modify(|s| s.lifecycle = Lifecycle::Spawning);

        // Report `Ready` only when the server has bound (fires `ReadySignal`),
        // not optimistically here — so readiness never claims the socket is
        // accepting before it is.
        let ready_status = self.status.clone();
        let ready = ReadySignal::new(move || {
            ready_status.send_modify(|s| {
                s.lifecycle = Lifecycle::Ready;
                s.health = Health::Healthy;
            });
        });

        let server = Arc::clone(&self.server);
        let status = self.status.clone();
        let task = Task::spawn(TaskName(self.name.0), move |cancel| async move {
            match server.serve(cancel, ready).await {
                // Clean shutdown after the token fired.
                Ok(()) => status.send_modify(|s| {
                    s.lifecycle = Lifecycle::Offline;
                    s.health = Health::Offline;
                }),
                // Bind failure (never became Ready) or a dead serve loop.
                Err(_) => status.send_modify(|s| s.health = Health::Critical),
            }
        });
        *self.task.lock().expect("serve task mutex poisoned") = Some(task);
        Ok(())
    }

    async fn restart(&self) -> Result<(), Self::Error> {
        self.stop().await?;
        self.spawn().await
    }

    async fn stop(&self) -> Result<(), Self::Error> {
        self.status
            .send_modify(|s| s.lifecycle = Lifecycle::Closing);
        let task = self.task.lock().expect("serve task mutex poisoned").take();
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
