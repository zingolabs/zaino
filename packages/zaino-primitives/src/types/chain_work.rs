//! Cumulative proof-of-work.

/// Cumulative chainwork at a block (256-bit big-endian).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainWork([u8; 32]);

impl ChainWork {
    /// Wrap raw chainwork bytes (256-bit big-endian).
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl From<ChainWork> for [u8; 32] {
    fn from(cw: ChainWork) -> Self {
        cw.0
    }
}

impl From<[u8; 32]> for ChainWork {
    fn from(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}
