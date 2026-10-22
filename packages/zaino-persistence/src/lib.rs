#![doc = include_str!("../usage.md")]
// production code never panics: no indexing, unchecked arithmetic or panicking calls
#![cfg_attr(
    not(test),
    deny(
        clippy::panic,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::string_slice,
        clippy::get_unwrap
    )
)]

mod backend;
mod error;
mod rules;
mod verify;

pub use backend::{
    raw_entry, sequence_key, sequence_position, Backend, BackendReader, BackendWriter,
    MaintenanceReader, Namespace, NamespaceKind, NamespaceSpec, RawKey, RawValue, ScanDirection,
    ScanRange, Width, WriteOp, SEQUENCE_KEY_LEN,
};
pub use error::{CommitError, FlushError, FootprintError, OpenError, Part, ReadError, VerifyError};
pub use rules::{
    check_commit, check_key, check_keys, check_shape, check_specs, CommitState, KeyState,
};
pub use verify::{NamespaceFootprint, Problem, Unit, Verification};

// test support (feature `testing`, never in production builds): asserting is its job
#[cfg(feature = "testing")]
#[allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
pub mod conformance;
#[cfg(feature = "testing")]
#[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
pub mod in_memory;
