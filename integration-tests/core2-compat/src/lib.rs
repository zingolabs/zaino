//! Compatibility shim for crates that still depend on `core2`.
//!
//! `core2` was yanked from crates.io. Zcash-maintained crates have moved to `corez`, so this local
//! crate preserves the old package name for transitive dependencies while delegating the API surface
//! to `corez`.

pub use corez::*;
