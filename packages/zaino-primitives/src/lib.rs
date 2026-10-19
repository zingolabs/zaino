//! Vocabulary every Zaino crate shares (crates depend on this, never on each other)
//!
//! - `types`: chain domain types (blocks, hashes, trees) + peer grouping; `protocol`: spec rules
//! - `display` (`ByteSize`, `Human`), `serde_secs`, [`sha256d`]; `testing`: regtest `MockChain`

use sha2::{Digest, Sha256};

pub mod display;
pub mod protocol;
pub mod serde_secs;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod types;

/// SHA-256 twice: block hashes, merkle nodes, pre-v5 txids
pub fn sha256d(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(Sha256::digest(bytes)).into()
}
