//! The reachability seam — a driven port for an observed dependency, checked by
//! a supervised observed component (e.g. the validator).

use core::future::Future;

/// A reachability check against an external dependency — the minimal thing the
/// runtime needs to gate bringup on a component it observes but does not own.
///
/// A *driven* port: the concrete client (e.g. a source client to the validator)
/// implements it; the runtime's observed component checks it. Lives here so a
/// domain crate can implement it depending only on `zaino-component`.
pub trait ReachabilityProbe {
    /// Whether the dependency is reachable right now.
    fn reachable(&self) -> impl Future<Output = bool> + Send;
}
