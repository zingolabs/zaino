//! Zaino primitives — vocabulary types for the Zcash chain.
//!
//! All Zaino crates that need chain-level types (heights, hashes) depend on this crate instead of
//! on each other.

use sha2::{Digest, Sha256};

pub mod network;
pub mod protocol;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod types;

/// SHA-256 twice: block hashes, merkle nodes, pre-v5 txids
pub fn sha256d(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(Sha256::digest(bytes)).into()
}
