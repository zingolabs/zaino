//! Thread-safe status wrappers.
//!
//! [`AtomicStatus`] and [`NamedAtomicStatus`] now live in `zaino-common` so that
//! lower-level crates (e.g. `zaino-mempool`) can share them without depending on
//! `zaino-state`. They are re-exported here to keep existing `crate::status::*`
//! call sites working unchanged.

pub use zaino_common::status::{AtomicStatus, NamedAtomicStatus, Status, StatusType};
