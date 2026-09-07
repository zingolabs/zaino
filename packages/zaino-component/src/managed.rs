//! Driving a component's lifecycle — the control side.

use core::future::Future;

/// Control over a component the runtime *owns*: it may spawn, restart, and stop
/// it.
///
/// Implementing this trait is the line between an **owned** component — the
/// runtime drives its lifecycle — and an **observed** one: an external
/// dependency (e.g. a validator) whose [`StatusSource`](crate::StatusSource) the
/// runtime reads but whose lifecycle it cannot drive, so it does not implement
/// this.
pub trait Managed {
    /// What can go wrong driving this component. Typed per implementor, so a
    /// management failure carries the component's own cause rather than a
    /// stringified one.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Start the component. Idempotent by contract: spawning a running component
    /// is a no-op, not a second instance.
    fn spawn(&self) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Stop the component and start it again.
    fn restart(&self) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Stop the component and release what it holds.
    fn stop(&self) -> impl Future<Output = Result<(), Self::Error>> + Send;
}
