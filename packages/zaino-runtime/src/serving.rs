//! A serve adapter as a supervised component.
//!
//! Wraps a serve adapter (the light-serve or node-rpc handler over one profile)
//! into a [`zaino_component`] the runtime can boot and supervise: it reports a
//! [`ComponentStatus`], publishes it on a `watch`, and is [`Managed`] (spawn /
//! restart / stop). The server's own status is gone — lifecycle and health are
//! owned here and observed by the Orchestra, replacing the hand-rolled
//! `NamedAtomicStatus` the old servers polled.
//!
//! The serve loop is a [`Task`] holding the adapter for its lifetime; in this
//! scaffold it simply serves until cancelled. A real transport server (tonic /
//! jsonrpsee) drops in as the task body, calling [`signal_health`] when its
//! serve task fails.
//!
//! [`signal_health`]: ServeComponent::signal_health

use std::sync::{Arc, Mutex};

use tokio::sync::watch;
use zaino_component::{
    ComponentName, ComponentStatus, Health, Lifecycle, Managed, StatusSource, StatusWatch, Task,
    TaskName,
};

/// A serve adapter of type `A`, presented to the runtime as a component.
pub struct ServeComponent<A> {
    name: ComponentName,
    adapter: Arc<A>,
    status: watch::Sender<ComponentStatus>,
    task: Arc<Mutex<Option<Task>>>,
}

impl<A> Clone for ServeComponent<A> {
    fn clone(&self) -> Self {
        Self {
            name: self.name,
            adapter: Arc::clone(&self.adapter),
            status: self.status.clone(),
            task: Arc::clone(&self.task),
        }
    }
}

impl<A> ServeComponent<A> {
    /// A component named `name` serving over `adapter`, initially `Offline`.
    pub fn new(name: ComponentName, adapter: A) -> Self {
        let (status, _) = watch::channel(ComponentStatus::new(
            name,
            Lifecycle::Offline,
            Health::Offline,
        ));
        Self {
            name,
            adapter: Arc::new(adapter),
            status,
            task: Arc::new(Mutex::new(None)),
        }
    }

    /// Flip the health condition — what a real serve task calls when it fails or
    /// recovers. (In this scaffold, the driver of an escalation in tests.)
    pub fn signal_health(&self, health: Health) {
        self.status.send_modify(|s| s.health = health);
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

impl<A: Send + Sync + 'static> Managed for ServeComponent<A> {
    // Bringing a server up cannot fail in this scaffold; a real transport bind
    // failure becomes a typed error here.
    type Error = std::convert::Infallible;

    async fn spawn(&self) -> Result<(), Self::Error> {
        self.status
            .send_modify(|s| s.lifecycle = Lifecycle::Spawning);
        let adapter = Arc::clone(&self.adapter);
        let task = Task::spawn(TaskName(self.name.0), move |cancel| async move {
            // Hold the adapter for the server's lifetime and serve until asked
            // to stop. A real transport server's accept loop lives here.
            let _serving = adapter;
            cancel.cancelled().await;
        });
        *self.task.lock().expect("serve task mutex poisoned") = Some(task);
        self.status.send_modify(|s| {
            s.lifecycle = Lifecycle::Ready;
            s.health = Health::Healthy;
        });
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
