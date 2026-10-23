//! Compact ciphertext: the scanning prefix of an encrypted note ciphertext

const COMPACT_CIPHERTEXT_LENGTH: usize = 52;
const NOTE_CIPHERTEXT_LENGTH: usize = 580;

/// First 52 bytes of a 580-byte note ciphertext (enough to trial-decrypt, whole note needs the
/// full transaction)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CompactCiphertext(pub [u8; COMPACT_CIPHERTEXT_LENGTH]);

impl CompactCiphertext {
    /// Infallible: the full ciphertext's width is in its type
    pub fn prefix_of(note: &[u8; NOTE_CIPHERTEXT_LENGTH]) -> Self {
        Self(core::array::from_fn(|i| note[i]))
    }
}
