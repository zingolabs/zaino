//! Compact ciphertext: the scanning prefix of an encrypted note ciphertext

const COMPACT_CIPHERTEXT_LENGTH: usize = 52;

/// Full Sapling / Orchard / Ironwood note ciphertext width
const NOTE_CIPHERTEXT_LENGTH: usize = 580;

/// First 52 bytes of a 580-byte note ciphertext (enough to trial-decrypt; the whole note needs
/// the full transaction)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CompactCiphertext(pub [u8; COMPACT_CIPHERTEXT_LENGTH]);

impl CompactCiphertext {
    pub const LENGTH: usize = COMPACT_CIPHERTEXT_LENGTH;

    /// Infallible: the full ciphertext's width is in its type
    pub fn prefix_of(note: &[u8; NOTE_CIPHERTEXT_LENGTH]) -> Self {
        Self(core::array::from_fn(|i| note[i]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_is_the_first_52_bytes() {
        let note: [u8; NOTE_CIPHERTEXT_LENGTH] = core::array::from_fn(|i| i as u8);
        let prefix = CompactCiphertext::prefix_of(&note).0;
        assert_eq!(prefix[..], note[..52]);
    }
}
