//! Zaino primitives — vocabulary types for the Zcash chain.
//!
//! All Zaino crates that need chain-level types (heights, hashes) depend on this crate instead of
//! on each other.

pub mod network;
pub mod protocol;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod types;
