//! Nullifier: marks a shielded note spent (Sapling, Orchard, Ironwood)

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Nullifier(pub [u8; 32]);
