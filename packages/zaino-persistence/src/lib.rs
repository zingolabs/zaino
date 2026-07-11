//! Driven port traits for index persistence.
//!
//! Defines the read/write interface that both the sync engine (writer)
//! and the serving layer (reader) depend on. Backend adapters (LMDB,
//! in-memory) implement these traits.

mod backend;
mod error;

pub use backend::{Backend, BackendReader, BackendWriter, Namespace, RawKey, RawValue, WriteOp};
pub use error::{CommitError, FlushError, OpenError, ReadError};

#[cfg(feature = "testing")]
pub mod in_memory;
