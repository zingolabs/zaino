//! Driving a component's lifecycle — the control side.

use core::future::Future;

/// Control over a component the runtime owns: it may spawn, restart, and stop
/// it.
///
/// Implementing this trait is what makes a component owned rather than
/// observed; see the crate documentation for that distinction.
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
