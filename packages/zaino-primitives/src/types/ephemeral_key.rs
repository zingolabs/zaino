//! Ephemeral key — used by recipients to detect and decrypt shielded notes.

/// An ephemeral public key (32 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EphemeralKey([u8; 32]);

impl From<[u8; 32]> for EphemeralKey {
    fn from(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl From<EphemeralKey> for [u8; 32] {
    fn from(k: EphemeralKey) -> Self {
        k.0
    }
}
