#![doc = include_str!("../usage.md")]
#![forbid(unsafe_code)]
#![deny(clippy::wildcard_enum_match_arm)]

mod error;
mod health;
mod lifecycle;
mod managed;
mod probe;
mod serve;
mod status;
mod sync;
mod task;

#[cfg(test)]
mod tests;

pub use error::error_chain;
pub use health::Health;
pub use lifecycle::{IllegalTransition, Lifecycle};
pub use managed::Managed;
pub use probe::ReachabilityProbe;
pub use serve::{ReadySignal, Serve};
pub use status::{ComponentName, ComponentStatus, StatusSource, StatusWatch};
pub use sync::SyncDriver;
pub use task::{Task, TaskError, TaskName};

// The cooperative-cancellation token a [`Task`] body receives, re-exported so a
// consumer naming it (e.g. a server's run signature) need not depend on
// `tokio-util` directly.
pub use tokio_util::sync::CancellationToken;
