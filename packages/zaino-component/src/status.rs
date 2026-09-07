//! What a component reports about itself — the read side.

use core::fmt;

use tokio::sync::watch;

use crate::{Health, Lifecycle};

/// A component's name, carried on its [`ComponentStatus`] so a transition logs
/// with a clean identifier of *which* component changed.
///
/// A newtype, distinct from [`TaskName`](crate::TaskName): a component and the
/// tasks it runs are different subjects, so mixing their names is a type error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ComponentName(pub &'static str);

impl fmt::Display for ComponentName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// A snapshot of a component's state: which component (`name`), its management
/// `lifecycle` phase, and its `health` condition.
///
/// A report only — transitions are owned by [`Lifecycle`], not by this bundle.
/// The two axes are independent: health is a condition, lifecycle a phase. A
/// `Copy` snapshot; [`StatusSource::status`] hands out a value, not a live
/// handle.
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
/// Universal to components; the runtime observes it. Cheap and synchronous —
/// reading a status must never await, so a supervisor can sample the whole
/// orchestra without yielding.
pub trait StatusSource {
    /// This component's current state.
    fn status(&self) -> ComponentStatus;
}

/// A component that publishes its status, so a supervisor can **react** to
/// changes instead of polling.
///
/// The receiver always holds the latest [`ComponentStatus`]; its `changed()`
/// wakes on each transition. This is the same `watch` shape the serviceability
/// manifest will consume, so a component publishes once and both read it.
pub trait StatusWatch {
    /// Subscribe to this component's status stream.
    fn subscribe(&self) -> watch::Receiver<ComponentStatus>;
}
