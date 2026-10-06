#![doc = include_str!("../usage.md")]

mod backend;
mod error;

pub use backend::{
    Backend, BackendReader, BackendWriter, KeyOrder, Namespace, NamespaceSpec, RangeVisitor,
    RawKey, RawValue, WriteOp,
};
pub use error::{CommitError, FlushError, OpenError, ReadError};

#[cfg(feature = "testing")]
pub mod in_memory;
