//! Nullifier — marks a shielded note as spent.

/// A nullifier (32 bytes). Used by both Sapling and Orchard pools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Nullifier([u8; 32]);

impl From<[u8; 32]> for Nullifier {
    fn from(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl From<Nullifier> for [u8; 32] {
    fn from(n: Nullifier) -> Self {
        n.0
    }
}
