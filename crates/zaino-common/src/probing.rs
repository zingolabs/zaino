//! Service health and readiness probing traits.
//!
//! This module provides decoupled traits for health and readiness checks,
//! following the Kubernetes probe model:
//!
//! - [`Liveness`]: Is the component alive and functioning?
//! - [`Readiness`]: Is the component ready to serve requests?
//! - [`VitalsProbe`]: Combined trait for components supporting both probes.
//!
//! These traits are intentionally simple (returning `bool`) and decoupled
//! from any specific status type, allowing flexible implementation across
//! different components.
//!
//! # Example
//!
//! ```
//! use zaino_common::probing::{Liveness, Readiness, VitalsProbe};
//!
//! struct MyService {
//!     connected: bool,
//!     synced: bool,
//! }
//!
//! impl Liveness for MyService {
//!     fn is_live(&self) -> bool {
//!         self.connected
//!     }
//! }
//!
//! impl Readiness for MyService {
//!     fn is_ready(&self) -> bool {
//!         self.connected && self.synced
//!     }
//! }
//!
//! // VitalsProbe is automatically implemented via blanket impl
//! fn check_service(service: &impl VitalsProbe) {
//!     println!("Live: {}, Ready: {}", service.is_live(), service.is_ready());
//! }
//! ```

/// Liveness probe: Is this component alive and functioning?
///
/// A component is considered "live" if it is not in a broken or
/// unrecoverable state. This corresponds to Kubernetes liveness probes.
///
/// Failure to be live typically means the component should be restarted.
pub trait Liveness {
    /// Returns `true` if the component is alive and functioning.
    fn is_live(&self) -> bool;
}

/// Readiness probe: Is this component ready to serve requests?
///
/// A component is considered "ready" if it can accept and process
/// requests. This corresponds to Kubernetes readiness probes.
///
/// A component may be live but not ready (e.g., still syncing).
pub trait Readiness {
    /// Returns `true` if the component is ready to serve requests.
    fn is_ready(&self) -> bool;
}

/// Combined vitals probe for components supporting both liveness and readiness.
///
/// This trait is automatically implemented for any type that implements
/// both [`Liveness`] and [`Readiness`].
pub trait VitalsProbe: Liveness + Readiness {}

// Blanket implementation: anything with Liveness + Readiness gets VitalsProbe
impl<T: Liveness + Readiness> VitalsProbe for T {}
