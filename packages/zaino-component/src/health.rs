//! A component's health.

/// How well a component is currently faring — a *condition*.
///
/// Orthogonal to its [`Lifecycle`](crate::Lifecycle): health can change on its
/// own at any moment (a dependency drops, a task panics), where lifecycle only
/// moves under management. The two are read independently; neither overwrites
/// the other.
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
