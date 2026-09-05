//! Compact ciphertext — the scanning prefix of an encrypted note ciphertext.

/// The compact-prefix width in bytes.
const COMPACT_CIPHERTEXT_LENGTH: usize = 52;

/// The first 52 bytes of an encrypted note ciphertext — the form a compact
/// transaction serves to light clients.
///
/// This is not the full encryption output: a Sapling or Orchard note
/// ciphertext is 580 bytes. The 52-byte head is enough for a wallet to trial
/// decrypt and detect a payment addressed to it; recovering the whole note
/// takes the full transaction. The width is enforced by construction — every
/// value of this type holds exactly 52 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CompactCiphertext([u8; COMPACT_CIPHERTEXT_LENGTH]);

/// Error when a byte string is not exactly the compact-prefix width.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("compact ciphertext must be exactly {COMPACT_CIPHERTEXT_LENGTH} bytes, got {got}")]
pub struct CompactCiphertextLength {
    /// The length that was rejected.
    pub got: usize,
}

impl CompactCiphertext {
    /// The compact-prefix width in bytes.
    pub const LENGTH: usize = COMPACT_CIPHERTEXT_LENGTH;

    /// Create from a byte slice of exactly [`Self::LENGTH`] bytes.
    pub fn try_new(bytes: &[u8]) -> Result<Self, CompactCiphertextLength> {
        <[u8; COMPACT_CIPHERTEXT_LENGTH]>::try_from(bytes)
            .map(Self)
            .map_err(|_| CompactCiphertextLength { got: bytes.len() })
    }
}

impl From<[u8; COMPACT_CIPHERTEXT_LENGTH]> for CompactCiphertext {
    fn from(bytes: [u8; COMPACT_CIPHERTEXT_LENGTH]) -> Self {
        Self(bytes)
    }
}

impl From<CompactCiphertext> for [u8; COMPACT_CIPHERTEXT_LENGTH] {
    fn from(c: CompactCiphertext) -> Self {
        c.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exactly_the_prefix_width_is_accepted_and_round_trips() {
        let bytes = [0xabu8; CompactCiphertext::LENGTH];

        let c = CompactCiphertext::try_new(&bytes).expect("exactly 52 bytes is valid");

        assert_eq!(<[u8; 52]>::from(c), bytes);
        assert_eq!(CompactCiphertext::from(bytes), c);
    }

    #[test]
    fn one_byte_short_is_rejected_with_the_length_seen() {
        let err = CompactCiphertext::try_new(&[0u8; 51]).expect_err("51 bytes is too short");

        assert_eq!(err, CompactCiphertextLength { got: 51 });
    }

    #[test]
    fn one_byte_long_is_rejected_with_the_length_seen() {
        let err = CompactCiphertext::try_new(&[0u8; 53]).expect_err("53 bytes is too long");

        assert_eq!(err, CompactCiphertextLength { got: 53 });
    }
}
