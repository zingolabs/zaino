#![doc = include_str!("../usage.md")]
#![forbid(unsafe_code)]
#![deny(clippy::wildcard_enum_match_arm)]

mod error;
mod health;
mod lifecycle;
mod managed;
mod probe;
mod run_loop;
mod status;

#[cfg(test)]
mod tests;

pub use error::error_chain;
pub use health::Health;
pub use lifecycle::{IllegalTransition, Lifecycle};
pub use managed::Managed;
pub use probe::ReachabilityProbe;
pub use run_loop::{RunLoop, RunReport, RunReporter};
pub use status::{ComponentName, ComponentStatus, Progress, StatusSource, StatusWatch};

// Tasks are the async layer below components: a supervised component *runs* on
// `zaino_async::Task`, it does not define it. The [`CancellationToken`] a
// component's trait signatures (`RunLoop`) take is re-exported from there so an
// implementor need name only `zaino-component`; `Task`/`TaskName`/`TaskError` are
// imported from `zaino-async` directly by whoever spawns.
pub use zaino_async::CancellationToken;
