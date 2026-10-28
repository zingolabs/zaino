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
//! So the dependency here is `tracing` and nothing else, and the crate stays
//! that way: this is vocabulary, not machinery.

pub mod probing;
pub mod status;

pub use probing::{Liveness, Readiness, VitalsProbe};
pub use status::{NamedAtomicStatus, Status, StatusType};
