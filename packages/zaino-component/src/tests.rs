//! The event → state → observe → act loop, and the lifecycle state machine.
//!
//! A component owns its own live state (no prescribed cell) and builds a
//! [`ComponentStatus`] snapshot — carrying its name — on demand. Its task fails
//! and flips health to `Critical` (event → state); a minimal supervisor reads
//! that through [`StatusSource`] (observe) and restarts it through [`Managed`]
//! (act). Separate tests pin the [`Lifecycle`] state machine and axis
//! independence.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::{
    ComponentName, ComponentStatus, Health, Lifecycle, Managed, StatusSource, Task, TaskName,
};

const NAME: ComponentName = ComponentName("flaky");

#[derive(Debug, thiserror::Error)]
#[error("work loop failed")]
struct WorkFailed;

/// The live state a component owns behind its own lock.
struct Live {
    lifecycle: Lifecycle,
    health: Health,
}

/// A component whose task marks it `Critical` while `fail` is set, `Healthy`
/// once cleared.
#[derive(Clone)]
struct Flaky {
    live: Arc<Mutex<Live>>,
    fail: Arc<AtomicBool>,
    task: Arc<Mutex<Option<Task>>>,
}

impl Flaky {
    fn new() -> Self {
        Self {
            live: Arc::new(Mutex::new(Live {
                lifecycle: Lifecycle::Offline,
                health: Health::Offline,
            })),
            fail: Arc::new(AtomicBool::new(true)),
            task: Arc::new(Mutex::new(None)),
        }
    }

    /// Advance the lifecycle through its own state machine.
    fn advance(&self, next: Lifecycle) {
        let mut live = self.live.lock().unwrap();
        live.lifecycle = live
            .lifecycle
            .try_advance_to(next)
            .expect("legal transition in mock");
    }

    fn set_health(&self, health: Health) {
        self.live.lock().unwrap().health = health;
    }

    fn start_task(&self) {
        let live = Arc::clone(&self.live);
        let fail = Arc::clone(&self.fail);
        let task = Task::spawn(TaskName("flaky-work"), move |cancel| async move {
            // The work's Err is an event that feeds the health state.
            let outcome: Result<(), WorkFailed> = if fail.load(Ordering::SeqCst) {
                Err(WorkFailed)
            } else {
                Ok(())
            };
            live.lock().unwrap().health = match outcome {
                Ok(()) => Health::Healthy,
                Err(_) => Health::Critical,
            };
            // Then idle, as a real work loop would, until told to stop.
            cancel.cancelled().await;
        });
        *self.task.lock().unwrap() = Some(task);
    }
}

impl StatusSource for Flaky {
    fn status(&self) -> ComponentStatus {
        let live = self.live.lock().unwrap();
        ComponentStatus::new(NAME, live.lifecycle, live.health)
    }
}

impl Managed for Flaky {
    type Error = std::convert::Infallible;

    async fn spawn(&self) -> Result<(), Self::Error> {
        self.advance(Lifecycle::Spawning);
        self.start_task();
        self.advance(Lifecycle::Ready);
        Ok(())
    }

    async fn restart(&self) -> Result<(), Self::Error> {
        self.stop().await?;
        self.spawn().await
    }

    async fn stop(&self) -> Result<(), Self::Error> {
        self.advance(Lifecycle::Closing);
        if let Some(task) = self.task.lock().unwrap().take() {
            task.abort();
        }
        self.set_health(Health::Offline);
        self.advance(Lifecycle::Offline);
        Ok(())
    }
}

/// Poll `cond` until true, or fail — the task sets health asynchronously.
async fn wait_until(mut cond: impl FnMut() -> bool) {
    for _ in 0..200 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("condition not met in time");
}

#[tokio::test]
async fn task_failure_flips_health_and_supervisor_restarts() {
    let component = Flaky::new();
    assert_eq!(component.status().health, Health::Offline, "not started");

    // Spawn → the task runs and fails: event → state.
    component.spawn().await.unwrap();
    wait_until(|| component.status().health == Health::Critical).await;
    assert_eq!(
        component.status().lifecycle,
        Lifecycle::Ready,
        "spawned regardless of health"
    );

    // A minimal supervisor: observe (StatusSource) then act (Managed).
    component.fail.store(false, Ordering::SeqCst);
    if component.status().health == Health::Critical {
        component.restart().await.unwrap();
    }
    wait_until(|| component.status().health == Health::Healthy).await;
    assert_eq!(component.status().lifecycle, Lifecycle::Ready);
    // The name rides along on the snapshot, for logging attribution.
    assert_eq!(component.status().name, NAME);
}

#[test]
fn lifecycle_forbids_nonsense_and_forces_restart_through_offline() {
    // Can't un-start a running component.
    let err = Lifecycle::Ready
        .try_advance_to(Lifecycle::Spawning)
        .unwrap_err();
    assert_eq!((err.from, err.to), (Lifecycle::Ready, Lifecycle::Spawning));

    // Ready may begin closing...
    assert_eq!(
        Lifecycle::Ready.try_advance_to(Lifecycle::Closing).unwrap(),
        Lifecycle::Closing
    );
    // ...but a closing component may not jump back to running, nor straight to
    // spawning — it must pass through Offline.
    assert!(Lifecycle::Closing.try_advance_to(Lifecycle::Ready).is_err());
    assert!(
        Lifecycle::Closing
            .try_advance_to(Lifecycle::Spawning)
            .is_err()
    );
    assert_eq!(
        Lifecycle::Closing
            .try_advance_to(Lifecycle::Offline)
            .unwrap(),
        Lifecycle::Offline
    );
    assert_eq!(
        Lifecycle::Offline
            .try_advance_to(Lifecycle::Spawning)
            .unwrap(),
        Lifecycle::Spawning
    );
}

#[test]
fn health_and_lifecycle_are_independent_axes() {
    // A snapshot can hold any (lifecycle, health) pair — the axes don't constrain
    // each other; only lifecycle *transitions* are constrained, and those live on
    // Lifecycle, not here.
    let status = ComponentStatus::new(NAME, Lifecycle::Ready, Health::Critical);
    assert_eq!(status.lifecycle, Lifecycle::Ready);
    assert_eq!(status.health, Health::Critical);
    assert_eq!(status.name, NAME);
}
