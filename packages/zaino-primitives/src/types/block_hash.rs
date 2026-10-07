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
        // first 4 display-order bytes (log readability)
        write!(
            f,
            "BlockHash({:02x}{:02x}{:02x}{:02x}…)",
            self.0[31], self.0[30], self.0[29], self.0[28]
        )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_is_all_zeroes() {
        assert_eq!(<[u8; 32]>::from(BlockHash::ZERO), [0u8; 32]);
    }

    #[test]
    fn roundtrip_bytes() {
        let bytes = [0xAB; 32];
        let hash = BlockHash::from(bytes);
        assert_eq!(<[u8; 32]>::from(hash), bytes);
    }

    #[test]
    fn display_is_big_endian_hex() {
        let mut bytes = [0u8; 32];
        bytes[31] = 0xAB;
        let hash = BlockHash::from(bytes);
        let display = format!("{hash}");
        assert!(display.starts_with("ab"), "got: {display}");
        assert_eq!(display.len(), 64);
    }

    #[test]
    fn debug_shows_truncated_prefix() {
        let mut bytes = [0u8; 32];
        bytes[31] = 0xDE;
        bytes[30] = 0xAD;
        let hash = BlockHash::from(bytes);
        let debug = format!("{hash:?}");
        assert!(debug.contains("dead"), "got: {debug}");
        assert!(debug.contains('…'), "got: {debug}");
    }

    #[test]
    fn equality_and_ordering() {
        let a = BlockHash::from([0u8; 32]);
        let b = BlockHash::from([1u8; 32]);
        assert_ne!(a, b);
        assert!(a < b);
    }
}
