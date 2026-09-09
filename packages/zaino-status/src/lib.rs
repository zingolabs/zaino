//! How a Zaino component reports whether it is working.
//!
//! [`StatusType`] is the vocabulary — the states a component can be in and how
//! two of them combine. [`Status`] is how a component reports one, and
//! [`Liveness`]/[`Readiness`] are the two questions an operator or orchestrator
//! actually asks, derived from it by blanket impl.
//!
//! # Why this is its own crate
//!
//! Status is the one thing *every* subsystem has, including the ones whose
//! whole purpose is to depend on as little as possible. Keeping this vocabulary
//! in a general-purpose crate meant reporting a status cost a dependency on
//! that crate's entire graph — the validator config, the logging stack, TLS,
//! `zebra-chain`. A subsystem should be able to say "I am syncing" without any
//! of that.
//!
//! Deps stay at `tracing` + (optional) the `metrics` facade — vocabulary, not
//! machinery

/// Prometheus metric names emitted by this crate; the single source of truth
/// shared with `zainod`'s `describe_*` registrations, which carry the
/// descriptions
#[allow(missing_docs)] // names are self-describing; descriptions live in zainod
pub mod metric_names {
    /// Emitted from the one call every status change passes through → the series set
    /// is the component set, with nothing to register. `zainod` registers it,
    /// numbering [`StatusType::ALL`](crate::StatusType::ALL) into the help text
    pub const STATUS: &str = "zaino.status";

    pub const STATUS_COMPONENT: &str = "component";
}

mod metric_macro;
pub mod probing;
pub mod status;
mod timer;

pub use timer::Timer;

#[doc(hidden)] // `timed!` reaches the facade through here, so callers need no dep
pub use metrics;

pub use probing::{Liveness, Readiness, VitalsProbe};
pub use status::{NamedAtomicStatus, Status, StatusType};
