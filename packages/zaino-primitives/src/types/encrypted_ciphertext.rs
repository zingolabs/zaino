//! Encrypted ciphertext — partial note encryption for wallet scanning.

/// First 52 bytes of an encrypted note ciphertext.
///
/// Enough for a wallet to attempt trial decryption and detect
/// payments. The full 580-byte ciphertext is not needed for scanning.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EncryptedCiphertext(Vec<u8>);

impl EncryptedCiphertext {
    /// Wrap ciphertext bytes.
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }
}

impl From<EncryptedCiphertext> for Vec<u8> {
    fn from(c: EncryptedCiphertext) -> Self {
        c.0
    }
}

impl From<Vec<u8>> for EncryptedCiphertext {
    fn from(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }
}
