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

/// A named snapshot of a component's [`Lifecycle`] phase and [`Health`]
/// condition, plus the cause of its current condition when it is not healthy.
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
}

impl ComponentStatus {
    /// A healthy status snapshot for `name` at `lifecycle` / `health`, with no
    /// failure cause. A failing supervisor sets [`reason`](Self::reason)
    /// directly at the boundary.
    pub fn new(name: ComponentName, lifecycle: Lifecycle, health: Health) -> Self {
        Self {
            name,
            lifecycle,
            health,
            reason: None,
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
        write!(f, "{}: {:?}/{:?}", self.name, self.lifecycle, self.health)?;
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
    use super::{ComponentName, ComponentStatus};
    use crate::{Health, Lifecycle};

    #[test]
    fn display_is_a_one_liner_with_the_cause_only_when_unhealthy() {
        let healthy =
            ComponentStatus::new(ComponentName("indexer"), Lifecycle::Ready, Health::Healthy);
        assert_eq!(healthy.to_string(), "indexer: Ready/Healthy");

        let mut failed = ComponentStatus::new(
            ComponentName("indexer"),
            Lifecycle::Syncing,
            Health::Critical,
        );
        failed.reason = Some("run loop panicked: boom".to_owned());
        assert_eq!(
            failed.to_string(),
            "indexer: Syncing/Critical — run loop panicked: boom"
        );
    }
}
