//! The orchestra: the runtime boots components in a dictated order and
//! supervises them.
//!
//! **Boot order** is the order components are handed to
//! [`OrchestraBuilder::boot`]: each is spawned and awaited to `Ready` before the
//! next begins, so a component others depend on (the validator) is up before
//! them. This is the seed of the dependency-ordered, readiness-gated bringup
//! (ADR-0014).
//!
//! Once booted, each component gets its own **babysitter** — a [`supervise`]
//! loop on a [`Task`] — and every babysitter reports escalations up **one shared
//! channel**, so the runtime learns *which* component went Critical without
//! holding a heterogeneous control-list. Observation is shared
//! (`dyn StatusSource`); control stays per-component (each babysitter is
//! monomorphised, so it calls the component's async methods with no boxing).

use std::sync::Arc;

use tokio::sync::mpsc;
use zaino_component::{
    ComponentName, ComponentStatus, Lifecycle, Managed, StatusSource, StatusWatch, Task, TaskName,
};

use crate::supervisor::{observe, supervise, RecoveryPolicy, SupervisionOutcome};

/// A component could not be booted.
#[derive(Debug, thiserror::Error)]
pub enum BootError<E: std::error::Error + Send + Sync + 'static> {
    /// The component's own `spawn` failed.
    #[error("component failed to spawn")]
    Spawn(#[source] E),
}

/// Boots components in order and wires their supervision.
pub struct OrchestraBuilder {
    escalations_tx: mpsc::UnboundedSender<ComponentName>,
    escalations_rx: mpsc::UnboundedReceiver<ComponentName>,
    babysitters: Vec<Task>,
    statuses: Vec<Arc<dyn StatusSource + Send + Sync>>,
}

impl OrchestraBuilder {
    /// A fresh builder, nothing booted yet.
    pub fn new() -> Self {
        let (escalations_tx, escalations_rx) = mpsc::unbounded_channel();
        Self {
            escalations_tx,
            escalations_rx,
            babysitters: Vec::new(),
            statuses: Vec::new(),
        }
    }

    /// Boot `component`: spawn it, wait until it is `Ready`, then start
    /// supervising it. Components boot in call order — each reaches `Ready`
    /// before the next begins.
    pub async fn boot<C>(mut self, component: C) -> Result<Self, BootError<C::Error>>
    where
        C: StatusSource + StatusWatch + Managed + Clone + Send + Sync + 'static,
    {
        component.spawn().await.map_err(BootError::Spawn)?;
        await_ready(&component).await;

        let name = component.status().name;
        let handle: Arc<dyn StatusSource + Send + Sync> = Arc::new(component.clone());
        self.statuses.push(handle);

        let escalations = self.escalations_tx.clone();
        let watched = component;
        let babysitter = Task::spawn(TaskName(name.0), move |cancel| async move {
            tokio::select! {
                _ = cancel.cancelled() => {}
                outcome = supervise(&watched, RecoveryPolicy::EscalateAll) => {
                    if matches!(outcome, Ok(SupervisionOutcome::Escalated)) {
                        // Report *which* component escalated; the runtime decides
                        // what that means (for now: treat as fatal).
                        let _ = escalations.send(watched.status().name);
                    }
                }
            }
        });
        self.babysitters.push(babysitter);
        Ok(self)
    }

    /// Boot an **observed** component: one the runtime does not own (no
    /// [`Managed`]) but gates bringup on and reacts to — the validator (ADR-0014).
    ///
    /// It is not spawned (it is external); we confirm it is `Ready`, then observe
    /// it, escalating on `Critical` exactly like an owned component but never
    /// restarting it. Because there is nothing to spawn, this cannot fail to
    /// boot — it returns `Self`, not a `Result`.
    pub async fn boot_observed<C>(mut self, component: C) -> Self
    where
        C: StatusSource + StatusWatch + Clone + Send + Sync + 'static,
    {
        await_ready(&component).await;

        let name = component.status().name;
        let handle: Arc<dyn StatusSource + Send + Sync> = Arc::new(component.clone());
        self.statuses.push(handle);

        let escalations = self.escalations_tx.clone();
        let watched = component;
        let babysitter = Task::spawn(TaskName(name.0), move |cancel| async move {
            tokio::select! {
                _ = cancel.cancelled() => {}
                outcome = observe(&watched) => {
                    if matches!(outcome, SupervisionOutcome::Escalated) {
                        let _ = escalations.send(watched.status().name);
                    }
                }
            }
        });
        self.babysitters.push(babysitter);
        self
    }

    /// Finish booting; hand back the running [`Orchestra`].
    pub fn build(self) -> Orchestra {
        Orchestra {
            babysitters: self.babysitters,
            statuses: self.statuses,
            escalations: self.escalations_rx,
        }
    }
}

impl Default for OrchestraBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// The booted, supervised set of components.
pub struct Orchestra {
    babysitters: Vec<Task>,
    statuses: Vec<Arc<dyn StatusSource + Send + Sync>>,
    escalations: mpsc::UnboundedReceiver<ComponentName>,
}

/// The result of running the orchestra to completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeOutcome {
    /// A component escalated. Under today's everything-fatal policy this brings
    /// the whole app down; the field names which component. (Later: only a
    /// *required* component is fatal — an optional one degrades and the app runs
    /// on.)
    Fatal {
        /// The component that escalated.
        component: ComponentName,
    },
    /// Every component stopped without escalating — a clean settle.
    Settled,
}

impl Orchestra {
    /// Run until a component escalates, then act on it.
    ///
    /// Current policy is everything-fatal: the first escalation shuts the rest
    /// down and returns [`RuntimeOutcome::Fatal`]. If every component stops
    /// without escalating, returns [`RuntimeOutcome::Settled`]. (Later this will
    /// loop, handling a non-fatal escalation per the component's role instead of
    /// tearing everything down.)
    pub async fn run(mut self) -> RuntimeOutcome {
        match self.next_escalation().await {
            Some(component) => {
                self.shutdown();
                RuntimeOutcome::Fatal { component }
            }
            None => RuntimeOutcome::Settled,
        }
    }

    /// The next component to escalate (go Critical), or `None` once every
    /// babysitter has stopped.
    pub async fn next_escalation(&mut self) -> Option<ComponentName> {
        self.escalations.recv().await
    }

    /// A snapshot of every component's status, in boot order.
    pub fn statuses(&self) -> Vec<ComponentStatus> {
        self.statuses.iter().map(|s| s.status()).collect()
    }

    /// Stop supervising every component.
    pub fn shutdown(&self) {
        for babysitter in &self.babysitters {
            babysitter.cancel();
        }
    }
}

/// Wait until `component` reports `Ready` (or goes away).
async fn await_ready<C: StatusWatch>(component: &C) {
    let mut status = component.subscribe();
    loop {
        if status.borrow_and_update().lifecycle == Lifecycle::Ready {
            return;
        }
        if status.changed().await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use tokio::sync::watch;
    use zaino_component::{
        ComponentName, ComponentStatus, Health, Lifecycle, Managed, StatusSource, StatusWatch,
    };

    use super::{OrchestraBuilder, RuntimeOutcome};

    /// A watch-backed component that records the order in which it is spawned.
    #[derive(Clone)]
    struct Mock {
        name: ComponentName,
        status: watch::Sender<ComponentStatus>,
        boot_log: Arc<Mutex<Vec<&'static str>>>,
    }

    impl Mock {
        fn new(name: &'static str, boot_log: Arc<Mutex<Vec<&'static str>>>) -> Self {
            let name = ComponentName(name);
            let (status, _) = watch::channel(ComponentStatus::new(
                name,
                Lifecycle::Offline,
                Health::Offline,
            ));
            Self {
                name,
                status,
                boot_log,
            }
        }

        fn set_health(&self, health: Health) {
            self.status.send_modify(|s| s.health = health);
        }
    }

    impl StatusSource for Mock {
        fn status(&self) -> ComponentStatus {
            *self.status.borrow()
        }
    }

    impl StatusWatch for Mock {
        fn subscribe(&self) -> watch::Receiver<ComponentStatus> {
            self.status.subscribe()
        }
    }

    impl Managed for Mock {
        type Error = std::convert::Infallible;

        async fn spawn(&self) -> Result<(), Self::Error> {
            self.boot_log.lock().unwrap().push(self.name.0);
            self.status.send_modify(|s| {
                s.lifecycle = Lifecycle::Ready;
                s.health = Health::Healthy;
            });
            Ok(())
        }

        async fn restart(&self) -> Result<(), Self::Error> {
            self.status.send_modify(|s| {
                s.lifecycle = Lifecycle::Ready;
                s.health = Health::Healthy;
            });
            Ok(())
        }

        async fn stop(&self) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn boots_in_order_and_funnels_escalations_by_name() {
        let boot_log = Arc::new(Mutex::new(Vec::new()));
        let validator = Mock::new("validator", boot_log.clone());
        let fs = Mock::new("fs", boot_log.clone());

        let mut orchestra = OrchestraBuilder::new()
            .boot(validator.clone())
            .await
            .unwrap()
            .boot(fs.clone())
            .await
            .unwrap()
            .build();

        // The runtime dictated the boot order: validator up before fs.
        assert_eq!(*boot_log.lock().unwrap(), vec!["validator", "fs"]);
        // Both booted through to Ready.
        let phases: Vec<_> = orchestra.statuses().iter().map(|s| s.lifecycle).collect();
        assert_eq!(phases, vec![Lifecycle::Ready, Lifecycle::Ready]);

        // fs goes Critical → its escalation funnels up, tagged with its name.
        fs.set_health(Health::Critical);
        let escalated = tokio::time::timeout(Duration::from_secs(1), orchestra.next_escalation())
            .await
            .expect("escalation arrived in time");
        assert_eq!(escalated, Some(ComponentName("fs")));

        orchestra.shutdown();
    }

    #[tokio::test]
    async fn an_escalation_is_fatal_and_names_the_component() {
        let boot_log = Arc::new(Mutex::new(Vec::new()));
        let validator = Mock::new("validator", boot_log.clone());
        let fs = Mock::new("fs", boot_log.clone());

        let orchestra = OrchestraBuilder::new()
            .boot(validator.clone())
            .await
            .unwrap()
            .boot(fs.clone())
            .await
            .unwrap()
            .build();

        // fs falls over → everything-fatal: the app comes down, naming fs.
        fs.set_health(Health::Critical);
        let outcome = tokio::time::timeout(Duration::from_secs(1), orchestra.run())
            .await
            .expect("orchestra ran to a decision");
        assert_eq!(
            outcome,
            RuntimeOutcome::Fatal {
                component: ComponentName("fs")
            }
        );
    }
}
