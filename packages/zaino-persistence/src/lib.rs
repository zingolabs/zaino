#![doc = include_str!("../usage.md")]

mod backend;
mod error;
mod rules;
mod verify;

pub use backend::{
    raw_entry, sequence_key, sequence_position, Backend, BackendReader, BackendWriter, Namespace,
    NamespaceKind, NamespaceSpec, RawKey, RawValue, ScanDirection, ScanRange, Width, WriteOp,
    SEQUENCE_KEY_LEN,
};
pub use error::{CommitError, FlushError, FootprintError, OpenError, Part, ReadError, VerifyError};
pub use rules::{check_commit, check_specs, CommitState, KeyState};
pub use verify::{NamespaceFootprint, Problem, Unit, Verification};

#[cfg(feature = "testing")]
pub mod conformance;
#[cfg(feature = "testing")]
pub mod in_memory;
