//! Capability: the port's health.

use std::future::Future;

use crate::error::PortError;

/// The port's readiness to serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// Serving: snapshots and subscriptions are available.
    Ready,
    /// Alive but not yet serving (e.g. still reaching a first
    /// consistent view).
    Starting,
}

/// Domain error for [`GetHealth`].
///
/// Empty: not-yet-serving is an answer ([`Health::Starting`]), not a
/// rejection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GetHealthError {}

/// The port's health signal (decision 7 of the design review — the
/// lifecycle surface is minimal: this and shutdown; construction stays
/// engine-specific in each driver's composition root).
pub trait GetHealth: Send + Sync {
    /// Whether the port is serving.
    fn get_health(&self) -> impl Future<Output = Result<Health, PortError<GetHealthError>>> + Send;
}
