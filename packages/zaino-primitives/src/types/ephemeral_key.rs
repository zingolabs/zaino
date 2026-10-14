//! Ephemeral public key: a recipient's handle to detect + decrypt a shielded note

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EphemeralKey(pub [u8; 32]);
