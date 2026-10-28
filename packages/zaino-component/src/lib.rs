#![doc = include_str!("../usage.md")]
#![forbid(unsafe_code)]
#![deny(clippy::wildcard_enum_match_arm)]

mod health;
mod lifecycle;
mod managed;
mod status;
mod task;

#[cfg(test)]
mod tests;

pub use health::Health;
pub use lifecycle::{IllegalTransition, Lifecycle};
pub use managed::Managed;
pub use status::{ComponentName, ComponentStatus, StatusSource, StatusWatch};
pub use task::{Task, TaskError, TaskName};
