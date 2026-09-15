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
/// condition.
///
/// A report only: transitions are owned by [`Lifecycle`], not by this bundle.
/// A `Copy` value, not a live handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ComponentStatus {
    /// Which component this is — for attribution and logs.
    pub name: ComponentName,
    /// The management phase.
    pub lifecycle: Lifecycle,
    /// The health condition.
    pub health: Health,
}

impl ComponentStatus {
    /// A status snapshot for `name` at `lifecycle` / `health`.
    pub fn new(name: ComponentName, lifecycle: Lifecycle, health: Health) -> Self {
        Self {
            name,
            lifecycle,
            health,
        }
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
