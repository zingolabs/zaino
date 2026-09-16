//! Re-exports from [`zaino_persistence`].
//!
//! The sync engine depends on the persistence driven port for its
//! storage interface. All backend types are defined in zaino-persistence;
//! this module re-exports them for convenience within zaino-sync.

pub use zaino_persistence::{
    Backend, BackendReader, BackendWriter, CommitError, FlushError, Namespace, OpenError, RawKey,
    RawValue, ReadError, WriteOp,
};
