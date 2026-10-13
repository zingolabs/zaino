//! SHA-256d block hash

use core::{fmt, str::FromStr};

use super::display_order::{self, ParseHashError};

/// Internal byte order (as the digest produces it); display / RPC reversed
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockHash([u8; 32]);

impl BlockHash {
    /// Sentinel (genesis `prev_hash`)
    pub const ZERO: Self = Self([0u8; 32]);
}

impl From<[u8; 32]> for BlockHash {
    fn from(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl From<BlockHash> for [u8; 32] {
    fn from(h: BlockHash) -> Self {
        h.0
    }
}

impl fmt::Debug for BlockHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        display_order::debug("BlockHash", &self.0, f)
    }
}

impl fmt::Display for BlockHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        display_order::write(&self.0, f)
    }
}

impl FromStr for BlockHash {
    type Err = ParseHashError;

    fn from_str(hex: &str) -> Result<Self, Self::Err> {
        display_order::parse(hex).map(Self)
    }
}
