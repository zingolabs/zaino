//! A **component**: a supervised in-process subsystem.
//!
//! Status is one *facet* of a component, not the whole of it. A component
//! standardises what every subsystem otherwise hand-rolls: its **health** (a
//! reported condition), its **lifecycle** (a management phase), its
//! **management** ([`Managed`]: spawn / restart / stop, for owned components),
//! and the **tasks** it runs ([`Task`]). Error bubbling and named logging fold
//! in here as the abstraction grows.
//!
//! Two altitudes: the low [`Task`] primitive (one supervised async task) and the
//! component itself (a subsystem owning one or more tasks). The read side
//! ([`StatusSource`]) and the control side ([`Managed`]) are separate
//! capabilities, so a consumer bounds on exactly what it uses: the runtime
//! *observes* every component but only *drives* the ones it owns — an observed
//! component (e.g. an external validator) reports a [`ComponentStatus`] without
//! implementing [`Managed`].
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
