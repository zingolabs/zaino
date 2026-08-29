//! The ChainHead runtime.
//!
//! [`ChainHeadService`] owns one writer task. It builds a complete window
//! before its constructor returns, then keeps that window reconciled with the
//! validator — extending it, rolling it back through reorgs, retaining the
//! competing branches the validator still knows about, and pruning below the
//! retention floor.
//!
//! Consumers get a [`ChainHeadSubscriber`], which produces published snapshots
//! and can do nothing else. There is no `sync`, no `reconcile`, no way to drive
//! or sequence synchronisation from outside: ChainHead synchronises itself, and
//! that is the whole point of it being a separate runtime.
//!
//! # The graph's representation lives here
//!
//! [`MapBackedSnapshot`] is this crate's implementation of
//! `zaino_chain_head::ChainHeadSnapshot`, and the trait is what consumers name.
//! Storing the graph differently — persistent structures sharing unchanged
//! subtrees between publishes, rather than maps cloned on each one — is a
//! change to this crate alone.
//!
//! # Publication is all-or-nothing
//!
//! A snapshot is built as a candidate and installed with one atomic store, and
//! only if the validator's tip did not move while it was being built. A reader
//! therefore never observes a half-applied reorg or a partially-filled window
//! — every published snapshot described the chain at some single instant.
//!
//! # Failure
//!
//! Construction is fallible; steady-state operation is not. A validator that
//! becomes unreachable leaves the last published snapshot in place and moves
//! the status to `RecoverableError`, then `CriticalError` once the failure
//! budget is spent. Stale data with a status saying so is more useful to a
//! consumer than no data at all.

#[cfg(feature = "prometheus")]
pub mod metric_names;

mod error;
mod service;
mod snapshot;
mod subscriber;

#[cfg(test)]
mod tests;

pub use error::{ChainHeadAdvanceError, ChainHeadInitError};
pub use service::ChainHeadService;
pub use snapshot::MapBackedSnapshot;
pub use subscriber::ChainHeadSubscriber;
