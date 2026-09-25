//! The runtime's cloud-native signals — modelled as a state machine.
//!
//! Two layers, kept separate on purpose:
//!
//! 1. **The state.** [`classify`] reduces the components' `Lifecycle`/`Health`
//!    to a single [`RuntimePhase`] — the app's macro-lifecycle. This is where
//!    config enters (via [`ReadinessCriteria`]); nothing else knows config.
//! 2. **The projection.** [`RuntimeSignals::from_phase`] maps a phase to the
//!    three probes by *exhaustive* match. The three booleans are a **lossy**
//!    projection of the phase — `Degraded` and `Draining` collapse to the same
//!    probe triple but are different states — so the phase carries more than the
//!    probes (useful for logs/metrics).
//!
//! The only memory is the startup latch (has it ever reached `Serving`), which is
//! what distinguishes `Booting` (never served) from `Degraded` (served, then a
//! component fell out). Everything else is a pure function of the current
//! component statuses.

use zaino_component::{ComponentStatus, Health, Lifecycle, Progress};

/// The default for [`ReadinessCriteria::max_blocks_behind`], equal to Zebra's `ready_max_blocks_behind`.
const DEFAULT_MAX_BLOCKS_BEHIND: u64 = 2;

/// The app's macro-lifecycle — the state the probes are projected from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimePhase {
    /// Never yet fully served; a required component is still coming up.
    Booting,
    /// Every required component is `Ready` and serving.
    Serving,
    /// Booted once, but a required component stopped serving (failed or
    /// re-syncing). Alive, out of rotation.
    Degraded,
    /// Shutting down; components are closing.
    Draining,
}

/// What "ready" requires — the config seam. Today only full mode exists; this is
/// where ephemeral / passthrough mode will set its knobs (e.g. not gating
/// readiness on a component that is still syncing).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadinessCriteria {
    /// Whether a component still `Syncing` blocks readiness. Full mode gates on
    /// sync; ephemeral mode (passthrough, no local index) does not.
    pub sync_gated: bool,
    /// The most blocks a component may trail its known target and still count as serving, or `None` to ignore lag.
    pub max_blocks_behind: Option<u64>,
}

impl Default for ReadinessCriteria {
    fn default() -> Self {
        // Full mode: a component that is still syncing is not yet serving.
        Self {
            sync_gated: true,
            max_blocks_behind: Some(DEFAULT_MAX_BLOCKS_BEHIND),
        }
    }
}

/// How a component's reported progress compares with [`ReadinessCriteria::max_blocks_behind`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lag {
    /// The criteria ignore lag, or the component reported no progress or no target.
    Unknown,
    /// The component trails its target by at most the tolerated number of blocks.
    Tolerated,
    /// The component trails its target by more than the tolerated number of blocks.
    Excessive,
}

impl ReadinessCriteria {
    /// Classifies `progress` against the lag tolerance.
    fn lag(&self, progress: Option<Progress>) -> Lag {
        let (
            Some(max),
            Some(Progress {
                current,
                target: Some(target),
            }),
        ) = (self.max_blocks_behind, progress)
        else {
            return Lag::Unknown;
        };
        if target.saturating_sub(current) <= max {
            Lag::Tolerated
        } else {
            Lag::Excessive
        }
    }
}

/// One component's contribution to the phase — an exhaustive classification of
/// its `(Lifecycle, Health)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Contribution {
    /// Serving requests now.
    Serving,
    /// Not serving yet (spawning / syncing / offline) — bringing up.
    ComingUp,
    /// Was expected to serve but cannot (critical / offline-while-Ready).
    Failed,
    /// Reached `Ready`, then fell further behind its target than the criteria tolerate.
    Lagging,
    /// Closing down.
    Draining,
}

/// Exhaustively classify a single component. No wildcards: a new `Lifecycle` or
/// `Health` variant must be decided here.
fn contribution(status: &ComponentStatus, criteria: &ReadinessCriteria) -> Contribution {
    let lag = criteria.lag(status.progress);
    match status.lifecycle {
        Lifecycle::Closing => Contribution::Draining,
        Lifecycle::Ready => match (status.health, lag) {
            (Health::Critical | Health::Offline, _) => Contribution::Failed,
            (Health::Healthy | Health::Recoverable, Lag::Excessive) => Contribution::Lagging,
            (Health::Healthy | Health::Recoverable, Lag::Unknown | Lag::Tolerated) => {
                Contribution::Serving
            }
        },
        // A syncing component blocks readiness only when the mode gates on sync;
        // ephemeral / passthrough mode does not.
        Lifecycle::Syncing => match (criteria.sync_gated, status.health, lag) {
            (false, _, _) => Contribution::Serving,
            (true, Health::Healthy | Health::Recoverable, Lag::Tolerated) => Contribution::Serving,
            (true, Health::Healthy | Health::Recoverable, Lag::Unknown | Lag::Excessive)
            | (true, Health::Critical | Health::Offline, _) => Contribution::ComingUp,
        },
        Lifecycle::Spawning | Lifecycle::Offline => Contribution::ComingUp,
    }
}

/// Reduce the (required) component statuses to a [`RuntimePhase`]. `started_latch`
/// carries whether the runtime has previously reached `Serving`, which decides
/// `Booting` vs `Degraded`.
pub fn classify(
    criteria: &ReadinessCriteria,
    statuses: &[ComponentStatus],
    started_latch: bool,
) -> RuntimePhase {
    if statuses.is_empty() {
        return RuntimePhase::Booting;
    }

    let mut any_draining = false;
    let mut all_serving = true;
    for status in statuses {
        match contribution(status, criteria) {
            Contribution::Serving => {}
            Contribution::ComingUp | Contribution::Failed | Contribution::Lagging => {
                all_serving = false
            }
            Contribution::Draining => {
                any_draining = true;
                all_serving = false;
            }
        }
    }

    if any_draining {
        RuntimePhase::Draining
    } else if all_serving {
        RuntimePhase::Serving
    } else if started_latch {
        RuntimePhase::Degraded
    } else {
        RuntimePhase::Booting
    }
}

/// The three standard probe signals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuntimeSignals {
    /// Startup complete — the runtime has reached `Serving` at least once.
    pub started: bool,
    /// The process is alive (not wedged). Independent of dependency health.
    pub live: bool,
    /// Ready to receive traffic.
    pub ready: bool,
}

impl RuntimeSignals {
    /// Project the phase onto the three probes — exhaustive by design.
    pub fn from_phase(phase: RuntimePhase) -> Self {
        match phase {
            RuntimePhase::Booting => Self {
                started: false,
                live: true,
                ready: false,
            },
            RuntimePhase::Serving => Self {
                started: true,
                live: true,
                ready: true,
            },
            // Degraded and Draining share the probe triple but are distinct phases.
            RuntimePhase::Degraded | RuntimePhase::Draining => Self {
                started: true,
                live: true,
                ready: false,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        classify, ReadinessCriteria, RuntimePhase, RuntimeSignals, DEFAULT_MAX_BLOCKS_BEHIND,
    };
    use zaino_component::{ComponentName, ComponentStatus, Health, Lifecycle, Progress};

    fn status(lifecycle: Lifecycle, health: Health) -> ComponentStatus {
        ComponentStatus::new(ComponentName("c"), lifecycle, health)
    }

    fn full() -> ReadinessCriteria {
        ReadinessCriteria::default()
    }

    #[test]
    fn empty_is_booting() {
        assert_eq!(classify(&full(), &[], false), RuntimePhase::Booting);
    }

    #[test]
    fn all_ready_is_serving() {
        let s = [
            status(Lifecycle::Ready, Health::Healthy),
            status(Lifecycle::Ready, Health::Recoverable),
        ];
        assert_eq!(classify(&full(), &s, false), RuntimePhase::Serving);
    }

    #[test]
    fn syncing_is_booting_before_first_serve() {
        let s = [status(Lifecycle::Syncing, Health::Healthy)];
        assert_eq!(classify(&full(), &s, false), RuntimePhase::Booting);
    }

    #[test]
    fn a_failure_after_boot_is_degraded_not_booting() {
        let s = [status(Lifecycle::Ready, Health::Critical)];
        assert_eq!(classify(&full(), &s, true), RuntimePhase::Degraded);
        assert_eq!(classify(&full(), &s, false), RuntimePhase::Booting);
    }

    #[test]
    fn closing_is_draining() {
        let s = [status(Lifecycle::Closing, Health::Healthy)];
        assert_eq!(classify(&full(), &s, true), RuntimePhase::Draining);
    }

    #[test]
    fn ephemeral_mode_does_not_gate_on_sync() {
        let s = [status(Lifecycle::Syncing, Health::Healthy)];
        let ephemeral = ReadinessCriteria {
            sync_gated: false,
            ..full()
        };
        assert_eq!(classify(&ephemeral, &s, false), RuntimePhase::Serving);
    }

    fn trailing(lifecycle: Lifecycle, blocks_behind: u64) -> ComponentStatus {
        const TARGET: u64 = 1_000;
        let mut trailing = status(lifecycle, Health::Healthy);
        trailing.progress = Some(Progress {
            current: TARGET - blocks_behind,
            target: Some(TARGET),
        });
        trailing
    }

    #[test]
    fn syncing_within_the_tolerance_is_serving() {
        let s = [trailing(Lifecycle::Syncing, DEFAULT_MAX_BLOCKS_BEHIND)];
        assert_eq!(classify(&full(), &s, false), RuntimePhase::Serving);
    }

    #[test]
    fn syncing_beyond_the_tolerance_is_booting() {
        let s = [trailing(Lifecycle::Syncing, DEFAULT_MAX_BLOCKS_BEHIND + 1)];
        assert_eq!(classify(&full(), &s, false), RuntimePhase::Booting);
    }

    #[test]
    fn ready_beyond_the_tolerance_is_degraded() {
        let s = [trailing(Lifecycle::Ready, DEFAULT_MAX_BLOCKS_BEHIND + 1)];
        assert_eq!(classify(&full(), &s, true), RuntimePhase::Degraded);
    }

    #[test]
    fn ready_within_the_tolerance_is_serving() {
        let s = [trailing(Lifecycle::Ready, DEFAULT_MAX_BLOCKS_BEHIND)];
        assert_eq!(classify(&full(), &s, true), RuntimePhase::Serving);
    }

    #[test]
    fn an_unknown_target_leaves_the_lifecycle_rule_in_force() {
        let mut s = [trailing(Lifecycle::Syncing, 0)];
        s[0].progress = Some(Progress {
            current: 1_000,
            target: None,
        });
        assert_eq!(classify(&full(), &s, false), RuntimePhase::Booting);
    }

    #[test]
    fn criteria_without_a_tolerance_ignore_lag() {
        let lag_blind = ReadinessCriteria {
            max_blocks_behind: None,
            ..full()
        };
        let far_behind = DEFAULT_MAX_BLOCKS_BEHIND + 1;
        let ready = [trailing(Lifecycle::Ready, far_behind)];
        assert_eq!(classify(&lag_blind, &ready, true), RuntimePhase::Serving);
        let syncing = [trailing(Lifecycle::Syncing, 0)];
        assert_eq!(classify(&lag_blind, &syncing, false), RuntimePhase::Booting);
    }

    #[test]
    fn an_unhealthy_syncing_component_gains_nothing_from_the_tolerance() {
        let mut s = [trailing(Lifecycle::Syncing, 0)];
        s[0].health = Health::Critical;
        assert_eq!(classify(&full(), &s, false), RuntimePhase::Booting);
    }

    #[test]
    fn projection_is_lossy_but_exhaustive() {
        assert!(!RuntimeSignals::from_phase(RuntimePhase::Booting).started);
        assert!(RuntimeSignals::from_phase(RuntimePhase::Serving).ready);
        // Degraded and Draining project to the same triple.
        assert_eq!(
            RuntimeSignals::from_phase(RuntimePhase::Degraded),
            RuntimeSignals::from_phase(RuntimePhase::Draining),
        );
        assert!(!RuntimeSignals::from_phase(RuntimePhase::Degraded).ready);
        assert!(RuntimeSignals::from_phase(RuntimePhase::Degraded).started);
    }
}
