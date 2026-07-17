//! Zaino primitives — vocabulary types for the Zcash chain.
//!
//! Zero-dependency crate. All Zaino crates that need chain-level types
//! (heights, hashes) depend on this crate instead of on each other.

#![forbid(unsafe_code)]

pub mod types;
