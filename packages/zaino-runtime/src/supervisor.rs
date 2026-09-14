//! Supervising a component: observe its health, act per policy.
//!
//! Mechanism and policy are separate. The **mechanism** is wired: the supervisor
//! can restart a component (via [`Managed`]) or escalate it to the runtime. The
//! **policy** ([`RecoveryPolicy`]) decides which. For now the runtime uses
//! [`RecoveryPolicy::EscalateAll`] — no restarts, every `Critical` bubbles
//! straight up — but [`RecoveryPolicy::RestartOnCritical`] exists and is
//! exercised, so turning recovery on later is a policy change, not new plumbing.
//!
//! [`Health::Recoverable`] already means "degraded but self-heals", so no policy
//! acts on it; only `Critical` does.
//!
//! [`Managed`]: zaino_component::Managed

use zaino_component::{Health, Managed, StatusSource, StatusWatch};

/// How the supervisor responds to a `Critical` component.
///
/// The recovery *mechanism* is always present; this selects whether to use it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryPolicy {
    /// No recovery: hand every `Critical` to the runtime (treated as fatal for
    /// now). The current default.
    EscalateAll,
    /// Restart a `Critical` component in place. (Later: bounded by a retry
    /// budget, then escalate on exhaustion.)
    RestartOnCritical,
}

impl RecoveryPolicy {
    fn action(self, health: Health) -> Action {
        match health {
            Health::Critical => match self {
                RecoveryPolicy::EscalateAll => Action::Escalate,
                RecoveryPolicy::RestartOnCritical => Action::Restart,
            },
            Health::Healthy | Health::Recoverable | Health::Offline => Action::Ignore,
        }
    }
}

/// The decision a policy makes for a given health — the supervisor's to execute.
enum Action {
    Ignore,
    Restart,
    Escalate,
}

/// What supervising a component actually did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupervisionOutcome {
    /// Health was acceptable; nothing was done.
    Observed,
    /// The component was restarted in place.
    Restarted,
    /// Health went `Critical` and the policy escalated it to the runtime.
    Escalated,
}

/// One supervision step: observe the component's health and act per `policy`.
pub async fn supervise_step<C>(
    component: &C,
    policy: RecoveryPolicy,
) -> Result<SupervisionOutcome, C::Error>
where
    C: StatusSource + Managed,
{
    match policy.action(component.status().health) {
        Action::Ignore => Ok(SupervisionOutcome::Observed),
        Action::Restart => {
            component.restart().await?;
            Ok(SupervisionOutcome::Restarted)
        }
        Action::Escalate => Ok(SupervisionOutcome::Escalated),
    }
}

/// Supervise `component` reactively until it escalates or goes away, sleeping
/// between transitions — no polling.
///
/// On each status change it acts per `policy`: restart in place and keep
/// watching, or escalate and return [`SupervisionOutcome::Escalated`] for the
/// runtime to act on. Returns [`SupervisionOutcome::Observed`] if the component
/// is dropped without ever escalating.
pub async fn supervise<C>(
    component: &C,
    policy: RecoveryPolicy,
) -> Result<SupervisionOutcome, C::Error>
where
    C: StatusWatch + Managed,
{
    let mut status = component.subscribe();
    loop {
        // Copy the health out so the watch borrow is dropped before any await.
        let health = status.borrow_and_update().health;
        match policy.action(health) {
            Action::Ignore => {}
            Action::Restart => component.restart().await?,
            Action::Escalate => return Ok(SupervisionOutcome::Escalated),
        }
        // Sleep until the next transition; `Err` means the component is gone.
        if status.changed().await.is_err() {
            return Ok(SupervisionOutcome::Observed);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::sync::watch;
    use zaino_component::{
        ComponentName, ComponentStatus, Health, Lifecycle, Managed, StatusSource, StatusWatch,
    };

    use super::{supervise, supervise_step, RecoveryPolicy, SupervisionOutcome};

    const NAME: ComponentName = ComponentName("mock");

    /// A watch-backed component: observable, subscribable, and restartable (the
    /// mechanism) — restarting heals it to Healthy/Ready and counts the restart.
    #[derive(Clone)]
    struct Mock {
        status: watch::Sender<ComponentStatus>,
        restarts: Arc<AtomicUsize>,
    }

    impl Mock {
        fn in_health(health: Health) -> Self {
            let (status, _) = watch::channel(ComponentStatus::new(NAME, Lifecycle::Ready, health));
            Self {
                status,
                restarts: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn set_health(&self, health: Health) {
            self.status.send_modify(|s| s.health = health);
        }

        fn restarts(&self) -> usize {
            self.restarts.load(Ordering::SeqCst)
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
            Ok(())
        }

        async fn restart(&self) -> Result<(), Self::Error> {
            self.status.send_modify(|s| {
                s.health = Health::Healthy;
                s.lifecycle = Lifecycle::Ready;
            });
            self.restarts.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn stop(&self) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn escalate_policy_bubbles_up_without_restarting() {
        let mock = Mock::in_health(Health::Critical);
        assert_eq!(
            supervise_step(&mock, RecoveryPolicy::EscalateAll)
                .await
                .unwrap(),
            SupervisionOutcome::Escalated
        );
        assert_eq!(mock.restarts(), 0, "escalate policy must not restart");
    }

    #[tokio::test]
    async fn restart_policy_exercises_the_mechanism() {
        let mock = Mock::in_health(Health::Critical);
        assert_eq!(
            supervise_step(&mock, RecoveryPolicy::RestartOnCritical)
                .await
                .unwrap(),
            SupervisionOutcome::Restarted
        );
        assert_eq!(mock.restarts(), 1);
        assert_eq!(mock.status().health, Health::Healthy);
    }

    #[tokio::test]
    async fn acceptable_health_is_observed_under_any_policy() {
        for policy in [
            RecoveryPolicy::EscalateAll,
            RecoveryPolicy::RestartOnCritical,
        ] {
            let mock = Mock::in_health(Health::Recoverable);
            assert_eq!(
                supervise_step(&mock, policy).await.unwrap(),
                SupervisionOutcome::Observed
            );
            assert_eq!(mock.restarts(), 0);
        }
    }

    #[tokio::test]
    async fn supervise_escalates_on_a_critical_transition() {
        let mock = Mock::in_health(Health::Healthy);
        let watched = mock.clone();
        let supervising =
            tokio::spawn(async move { supervise(&watched, RecoveryPolicy::EscalateAll).await });

        // A self-healing wobble must not escalate; a Critical must.
        mock.set_health(Health::Recoverable);
        mock.set_health(Health::Critical);

        let outcome = tokio::time::timeout(Duration::from_secs(1), supervising)
            .await
            .expect("supervise returned in time")
            .expect("supervise task ok")
            .unwrap();
        assert_eq!(outcome, SupervisionOutcome::Escalated);
        assert_eq!(mock.restarts(), 0);
    }
}
