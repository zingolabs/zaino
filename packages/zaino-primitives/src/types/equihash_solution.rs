//! Equihash solution as a block header carries it

/// Carried, never validated (the block hash commits to it: dropping it = a lossy block)
///
/// - `Standard` = 200-9 (mainnet / testnet), `Regtest` = 48-5; the length tells them apart
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
// 1344-byte `Standard` dominates the size; boxing = a per-block heap allocation for nothing
#[allow(clippy::large_enum_variant)]
pub enum EquihashSolution {
    Standard([u8; 1344]),
    Regtest([u8; 36]),
}

impl EquihashSolution {
    /// Without the wire encoding's length prefix
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Standard(bytes) => bytes,
            Self::Regtest(bytes) => bytes,
        }
    }
}
