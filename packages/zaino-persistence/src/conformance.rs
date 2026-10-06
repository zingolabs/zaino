//! A generic conformance suite that pins the [`Backend`] contract from outside
//! and runs against any backend.
//!
//! Each property is a `pub fn` taking a [`BackendFactory`]; a backend crate wires
//! them into its own `#[test]`s (one per property, for a precise failure) or
//! calls [`run_all`] for all of them. The suite asserts real semantics with
//! discriminating data — keys whose bytewise order differs from both their
//! insertion order and their numeric order, ranges that probe each bound's
//! inclusivity, multi-namespace atomicity — and holds for a backend that ignores
//! bulk mode as well as one that defers.
//!
//! Behind the `testing` feature. To run it, implement [`BackendFactory`] over
//! your backend:
//!
//! ```rust,ignore
//! use zaino_persistence::conformance::{self, BackendFactory};
//! use zaino_persistence::NamespaceSpec;
//!
//! struct MyFactory { /* e.g. a temp dir */ }
//!
//! impl BackendFactory for MyFactory {
//!     type B = MyBackend;
//!     fn fresh(&self, namespaces: &[NamespaceSpec]) -> Self::B { /* open empty */ }
//!     fn reopen(&self, namespaces: &[NamespaceSpec]) -> Option<Self::B> {
//!         // Some(reopen the same storage) if persistent, else None.
//!     }
//! }
//!
//! #[test]
//! fn get_put_delete_round_trip() {
//!     conformance::get_put_delete_round_trip(&MyFactory::new());
//! }
//! ```

mod basic;
mod bulk;
mod support;

pub use basic::{
    commit_is_atomic_across_namespaces, first_key_is_smallest_or_none, get_put_delete_round_trip,
    namespaces_are_isolated, reopen_persists_committed_data, scan_range_is_ascending_and_half_open,
    scan_returns_bytewise_key_order,
};
pub use bulk::{
    bulk_disabled_matches_direct, bulk_enabled_after_finish_matches_direct,
    finish_bulk_is_idempotent, is_complete_inside_bulk_mode, is_complete_true_outside_bulk_mode,
    restart_in_bulk_mode_matches_direct,
};

use crate::backend::{Backend, NamespaceSpec};

/// Builds the backends the conformance suite drives.
///
/// `&self` so one factory serves a whole run: each property opens its own
/// [`fresh`](Self::fresh) backend, and a persistent factory remembers where the
/// last `fresh` put its storage so [`reopen`](Self::reopen) can reopen it. A
/// non-persistent backend returns `None` from `reopen`, and the persistence
/// properties skip themselves.
pub trait BackendFactory {
    /// The backend under test.
    type B: Backend;

    /// Open a fresh, empty backend with these namespaces.
    fn fresh(&self, namespaces: &[NamespaceSpec]) -> Self::B;

    /// Reopen the storage the most recent [`fresh`](Self::fresh) created, after
    /// its handle has been dropped (to prove durability across a restart).
    /// Returns `None` for a non-persistent backend.
    fn reopen(&self, namespaces: &[NamespaceSpec]) -> Option<Self::B>;
}

/// Run every conformance property against `factory`.
///
/// A backend crate preferring one aggregate test over one test per property can
/// call this; a precise-failure wiring calls each `pub fn` from its own
/// `#[test]`. Persistence and restart properties no-op when `reopen` is `None`.
pub fn run_all<F: BackendFactory>(factory: &F) {
    get_put_delete_round_trip(factory);
    commit_is_atomic_across_namespaces(factory);
    scan_returns_bytewise_key_order(factory);
    scan_range_is_ascending_and_half_open(factory);
    first_key_is_smallest_or_none(factory);
    namespaces_are_isolated(factory);
    reopen_persists_committed_data(factory);

    bulk_disabled_matches_direct(factory);
    bulk_enabled_after_finish_matches_direct(factory);
    is_complete_true_outside_bulk_mode(factory);
    is_complete_inside_bulk_mode(factory);
    finish_bulk_is_idempotent(factory);
    restart_in_bulk_mode_matches_direct(factory);
}
