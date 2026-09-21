//! A component's health.

/// How well a component is currently faring — a *condition*.
///
/// One of the two axes of a [`ComponentStatus`](crate::ComponentStatus); see
/// the crate documentation for how it relates to
/// [`Lifecycle`](crate::Lifecycle).
#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::Display)]
pub enum Health {
    /// Working normally.
    Healthy,
    /// Degraded but expected to recover on its own; no operator action yet.
    Recoverable,
    /// Broken in a way it cannot recover from unaided.
    Critical,
    /// Not running.
    Offline,
}
