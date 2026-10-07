//! Nullifier: marks a shielded note spent (Sapling, Orchard, Ironwood)

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
