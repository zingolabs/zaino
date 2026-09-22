//! What a component reports about itself — the read side.

use core::fmt;

use tokio::sync::watch;

use crate::{Health, Lifecycle};

/// A component's name, carried on its [`ComponentStatus`] so a transition logs
/// with a clean identifier of *which* component changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ComponentName(pub &'static str);

impl fmt::Display for ComponentName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// A component's position toward a target — e.g. an indexer's committed height
/// toward the chain tip.
///
/// Domain-free by design: a plain count, not a domain `Height`, so this crate
/// stays free of chain vocabulary. A reporter maps its domain position onto these
/// counts (see [`crate::RunReporter::progress`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress {
    /// How far the component has got.
    pub current: u64,
    /// The target it is working toward, if known.
    pub target: Option<u64>,
}

impl Progress {
    /// Progress toward the target in per-mille (0–1000 ≙ 0–100%), when the target
    /// is known and non-zero. Integer math via `u128` so a large count cannot
    /// overflow the multiply and no lossy float conversion is needed; the caller
    /// splits it into whole percent and a tenth for display.
    fn per_mille(&self) -> Option<u128> {
        let target = self.target.filter(|t| *t > 0)?;
        Some(u128::from(self.current) * 1000 / u128::from(target))
    }
}

impl fmt::Display for Progress {
    /// `current/target`, thousands-separated for legibility
    /// (`1,700,000/3,428,000`); the target is `?` when unknown.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/", group_thousands(self.current))?;
        match self.target {
            Some(target) => f.write_str(&group_thousands(target)),
            None => f.write_str("?"),
        }
    }
}

/// Render `n` with `,` thousands separators (`1700000` → `1,700,000`) — raw
/// block heights are hard to read at a glance.
fn group_thousands(n: u64) -> String {
    let digits = n.to_string();
    let len = digits.len();
    let mut out = String::with_capacity(len + len / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A named snapshot of a component's [`Lifecycle`] phase and [`Health`]
/// condition, the cause of its current condition when it is not healthy, and its
/// progress toward a target when it has one.
///
/// A report only: transitions are owned by [`Lifecycle`], not by this bundle.
/// Not `Copy` — it carries an owned [`reason`](Self::reason) string so a
/// supervisor (and any health endpoint) can report *why* a component is
/// `Critical`, not merely *that* it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComponentStatus {
    /// Which component this is — for attribution and logs.
    pub name: ComponentName,
    /// The management phase.
    pub lifecycle: Lifecycle,
    /// The health condition.
    pub health: Health,
    /// The human-readable cause of the current condition — the failing error's
    /// full source chain when the component is unhealthy, `None` when healthy.
    /// Set at the supervision boundary; cleared on any clean transition so a
    /// stale cause never lingers past recovery.
    pub reason: Option<String>,
    /// The component's position toward a target, when it reports one (e.g. an
    /// indexer's committed height toward the tip). `None` for components with no
    /// meaningful progress (e.g. a bound server).
    pub progress: Option<Progress>,
}

impl ComponentStatus {
    /// A healthy status snapshot for `name` at `lifecycle` / `health`, with no
    /// failure cause and no progress. A failing supervisor sets
    /// [`reason`](Self::reason) directly at the boundary; a running component
    /// reports [`progress`](Self::progress) through its [`crate::RunReporter`].
    pub fn new(name: ComponentName, lifecycle: Lifecycle, health: Health) -> Self {
        Self {
            name,
            lifecycle,
            health,
            reason: None,
            progress: None,
        }
    }
}

/// The canonical one-line human rendering: `name: lifecycle/health`, plus the
/// cause when the component is unhealthy. The single form logs, a health
/// endpoint, and a CLI should reuse instead of each re-formatting the fields;
/// [`Debug`] stays the structural, developer view. An aligned table across many
/// components is a *collection* concern, not this per-value form.
impl fmt::Display for ComponentStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {:?} | {:?}", self.name, self.lifecycle, self.health)?;
        if let Some(progress) = &self.progress {
            match progress.per_mille() {
                // `495` per-mille → `49.5%`; the percent leads so a glance reads
                // "how far" before the raw counts.
                Some(pm) => write!(f, " · sync progress: {}.{}% ({progress})", pm / 10, pm % 10)?,
                None => write!(f, " · sync progress: ({progress})")?,
            }
        }
        if let Some(reason) = &self.reason {
            write!(f, " — {reason}")?;
        }
        Ok(())
    }
}

/// Anything that reports a [`ComponentStatus`].
///
/// Cheap and synchronous by contract: reading a status must never await, so a
/// supervisor can sample every component without yielding.
pub trait StatusSource {
    /// This component's current state.
    fn status(&self) -> ComponentStatus;
}

/// A component that publishes its status, so a supervisor can react to changes
/// instead of polling.
///
/// The receiver always holds the latest [`ComponentStatus`]; its `changed()`
/// wakes on each transition.
pub trait StatusWatch {
    /// Subscribe to this component's status stream.
    fn subscribe(&self) -> watch::Receiver<ComponentStatus>;
}

#[cfg(test)]
mod tests {
    use super::{ComponentName, ComponentStatus, Progress};
    use crate::{Health, Lifecycle};

    #[test]
    fn display_is_a_one_liner_with_the_cause_only_when_unhealthy() {
        let healthy =
            ComponentStatus::new(ComponentName("indexer"), Lifecycle::Ready, Health::Healthy);
        assert_eq!(healthy.to_string(), "indexer: Ready | Healthy");

        let mut failed = ComponentStatus::new(
            ComponentName("indexer"),
            Lifecycle::Syncing,
            Health::Critical,
        );
        failed.reason = Some("run loop panicked: boom".to_owned());
        assert_eq!(
            failed.to_string(),
            "indexer: Syncing | Critical — run loop panicked: boom"
        );
    }

    #[test]
    fn display_shows_progress_when_present() {
        let mut syncing = ComponentStatus::new(
            ComponentName("indexer"),
            Lifecycle::Syncing,
            Health::Healthy,
        );
        syncing.progress = Some(Progress {
            current: 1_700_000,
            target: Some(3_428_000),
        });
        assert_eq!(
            syncing.to_string(),
            "indexer: Syncing | Healthy · sync progress: 49.5% (1,700,000/3,428,000)"
        );

        // Target unknown: no percentage, and the target renders as `?`.
        syncing.progress = Some(Progress {
            current: 1_700_000,
            target: None,
        });
        assert_eq!(
            syncing.to_string(),
            "indexer: Syncing | Healthy · sync progress: (1,700,000/?)"
        );
    }

    #[test]
    fn progress_percentage_renders_to_a_tenth_and_guards_zero_target() {
        let pct = |current, target| {
            let mut s = ComponentStatus::new(
                ComponentName("indexer"),
                Lifecycle::Syncing,
                Health::Healthy,
            );
            s.progress = Some(Progress { current, target });
            s.to_string()
        };
        // Whole and fractional percents, truncated to a tenth.
        assert!(pct(0, Some(1_000)).ends_with("0.0% (0/1,000)"));
        assert!(pct(500, Some(1_000)).ends_with("50.0% (500/1,000)"));
        assert!(pct(3_428_000, Some(3_428_000)).ends_with("100.0% (3,428,000/3,428,000)"));
        // 1/3 → 33.3% (truncated, not rounded).
        assert!(pct(1, Some(3)).ends_with("33.3% (1/3)"));
        // A zero target can't yield a percentage — fall back to the bare fraction.
        assert!(pct(5, Some(0)).ends_with("sync progress: (5/0)"));
    }

    #[test]
    fn thousands_separators_group_by_three() {
        for (n, expect) in [
            (0u64, "0"),
            (7, "7"),
            (42, "42"),
            (999, "999"),
            (1_000, "1,000"),
            (12_345, "12,345"),
            (100_000, "100,000"),
            (3_428_143, "3,428,143"),
        ] {
            assert_eq!(super::group_thousands(n), expect, "grouping {n}");
        }
    }
}
