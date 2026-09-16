//! The runtime's cloud-native signals — an edge-only projection of the
//! components' statuses.
//!
//! The Orchestra holds the rich per-component `Lifecycle`/`Health`; a deployment
//! wants the three coarse probes. These are projections of the aggregate, so the
//! probe answer always reflects the orchestration:
//!
//! - **startup** — has the runtime finished booting? Latches true once every
//!   component has reached `Ready`; it does not flap back (a component that
//!   re-syncs is a *readiness* change, not an un-boot).
//! - **liveness** — is the process alive / not wedged? Deliberately coarse and
//!   dependency-independent: a dropped dependency makes the runtime *not ready*,
//!   never *not alive* (else an external outage would trigger restarts).
//! - **readiness** — should it receive traffic now? Every component `Ready` and
//!   serving (`Healthy` or `Recoverable`); false while any is `Critical`,
//!   `Offline`, or still bringing up / draining.

use zaino_component::{ComponentStatus, Health, Lifecycle};

/// The three standard probe signals, projected from the component statuses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuntimeSignals {
    /// Startup complete — every component has reached `Ready` at least once.
    pub started: bool,
    /// The process is alive (not wedged). Independent of dependency health.
    pub live: bool,
    /// Ready to receive traffic — every component `Ready` and serving.
    pub ready: bool,
}

impl RuntimeSignals {
    /// Project the signals from the current component statuses. `started_latch`
    /// carries the previous `started` value so startup latches true and does not
    /// flap.
    pub fn project(statuses: &[ComponentStatus], started_latch: bool) -> Self {
        let all_ready =
            !statuses.is_empty() && statuses.iter().all(|s| s.lifecycle == Lifecycle::Ready);
        let all_serving = all_ready
            && statuses
                .iter()
                .all(|s| matches!(s.health, Health::Healthy | Health::Recoverable));
        Self {
            started: started_latch || all_ready,
            // Live while the runtime is running to project this at all. A dead or
            // wedged runtime simply stops updating / answering.
            live: true,
            ready: all_serving,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RuntimeSignals;
    use zaino_component::{ComponentName, ComponentStatus, Health, Lifecycle};

    fn status(lifecycle: Lifecycle, health: Health) -> ComponentStatus {
        ComponentStatus::new(ComponentName("c"), lifecycle, health)
    }

    #[test]
    fn empty_is_not_started_or_ready() {
        let s = RuntimeSignals::project(&[], false);
        assert!(!s.started && !s.ready && s.live);
    }

    #[test]
    fn all_ready_and_healthy_is_started_and_ready() {
        let s = RuntimeSignals::project(
            &[
                status(Lifecycle::Ready, Health::Healthy),
                status(Lifecycle::Ready, Health::Healthy),
            ],
            false,
        );
        assert!(s.started && s.ready);
    }

    #[test]
    fn a_critical_component_is_started_but_not_ready() {
        // Latched started, one component Critical -> booted but out of rotation.
        let s = RuntimeSignals::project(&[status(Lifecycle::Ready, Health::Critical)], true);
        assert!(s.started);
        assert!(!s.ready);
    }

    #[test]
    fn recoverable_still_serves() {
        let s = RuntimeSignals::project(&[status(Lifecycle::Ready, Health::Recoverable)], false);
        assert!(s.ready, "degraded-but-self-healing stays in rotation");
    }

    #[test]
    fn still_syncing_is_not_ready_and_startup_does_not_latch() {
        let s = RuntimeSignals::project(&[status(Lifecycle::Syncing, Health::Healthy)], false);
        assert!(!s.started && !s.ready);
    }
}
