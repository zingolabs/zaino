//! A supervised, long-running loop as a runtime component.
//!
//! An index **writer** (sources blocks, builds the index) and a network
//! **server** (binds, serves) are the same shape — a long-lived fallible loop
//! that signals readiness once and runs until cancelled — so one component drives
//! both. [`RunComponent`] owns a [`RunLoop`]'s lifecycle: it reports a
//! [`ComponentStatus`], publishes it on a `watch`, is [`Managed`] (the runtime
//! spawns / restarts / stops it), and reconciles the loop's terminal outcome to
//! status through the shared [`crate::run`] seam.
//!
//! The one behavioural axis between roles is [`RunLoop::RUNNING`], the lifecycle
//! while the loop is running-but-not-yet-`Ready`: a writer goes `Syncing` (it
//! already serves dependents while catching up, so the Orchestra boots them once
//! it is running), a server stays `Spawning` (it must bind before it is `Ready`).
//! Readiness is reported by the loop firing its [`ReadySignal`], never
//! optimistically at spawn. Health is driven by the loop's outcome: a clean stop
//! settles `Offline`, a failure (bind failure, dead loop, panic) flips `Critical`
//! with the cause, which the Orchestra escalates.

use std::sync::{Arc, Mutex};

use tokio::sync::watch;
use zaino_async::{Task, TaskName};
use zaino_component::{
    ComponentName, ComponentStatus, Health, Lifecycle, Managed, ReadySignal, RunLoop, StatusSource,
    StatusWatch,
};

/// A [`RunLoop`] `R`, presented to the runtime as a supervised component.
pub struct RunComponent<R> {
    name: ComponentName,
    runnable: Arc<R>,
    status: watch::Sender<ComponentStatus>,
    task: Arc<Mutex<Option<Task>>>,
}

impl<R> Clone for RunComponent<R> {
    fn clone(&self) -> Self {
        Self {
            name: self.name,
            runnable: Arc::clone(&self.runnable),
            status: self.status.clone(),
            task: Arc::clone(&self.task),
        }
    }
}

impl<R> RunComponent<R> {
    /// A component named `name` supervising `runnable`, initially `Offline`.
    pub fn new(name: ComponentName, runnable: R) -> Self {
        let (status, _) = watch::channel(ComponentStatus::new(
            name,
            Lifecycle::Offline,
            Health::Offline,
        ));
        Self {
            name,
            runnable: Arc::new(runnable),
            status,
            task: Arc::new(Mutex::new(None)),
        }
    }
}

impl<R: Send + Sync + 'static> StatusSource for RunComponent<R> {
    fn status(&self) -> ComponentStatus {
        self.status.borrow().clone()
    }
}

impl<R: Send + Sync + 'static> StatusWatch for RunComponent<R> {
    fn subscribe(&self) -> watch::Receiver<ComponentStatus> {
        self.status.subscribe()
    }
}

impl<R: RunLoop> Managed for RunComponent<R> {
    // Starting the loop never fails synchronously: a bind/source failure surfaces
    // as the run task's early `Err`, which flips health `Critical` for the
    // Orchestra to escalate — the same reactive path as a mid-run failure.
    type Error = std::convert::Infallible;

    async fn spawn(&self) -> Result<(), Self::Error> {
        self.status
            .send_modify(|s| s.lifecycle = Lifecycle::Spawning);

        // Report `Ready` only when the loop fires its readiness signal (bound /
        // caught up), not optimistically here.
        let ready_status = self.status.clone();
        let ready = ReadySignal::new(move || {
            ready_status.send_modify(|s| {
                s.lifecycle = Lifecycle::Ready;
                s.health = Health::Healthy;
                s.reason = None;
            });
        });

        let runnable = Arc::clone(&self.runnable);
        let run_status = self.status.clone();
        let name = self.name;
        let task = Task::spawn(TaskName(self.name.0), move |cancel| async move {
            // Now actively running: enter the role's running phase (`Syncing` for a
            // writer, `Spawning` for a server) until the loop signals `Ready`.
            run_status.send_modify(|s| {
                s.lifecycle = R::RUNNING;
                s.health = Health::Healthy;
                s.reason = None;
            });
            // The run boundary — catch a panic, log any failure, reconcile the
            // terminal status — is one shared seam (see `crate::run`), so no
            // component re-implements the match nor forgets to log.
            crate::run::run_and_reconcile(name, &run_status, R::LABEL, runnable.run(cancel, ready))
                .await;
        });
        *self.task.lock().expect("run component task mutex poisoned") = Some(task);
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
            .expect("run component task mutex poisoned")
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
