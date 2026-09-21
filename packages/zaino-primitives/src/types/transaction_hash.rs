//! Transaction hash (txid).

use core::fmt;

/// Transaction hash / txid (32 bytes, internal byte order).
///
/// Same byte-order convention as [`super::BlockHash`]: internal
/// little-endian, display big-endian.
///
/// NOTE: Transaction hash vs transaction ID
/// - In pre V5 transactions this is the transaction hash (sha256 of serialized tx).
/// - From V5 onwards this field is the transaction ID (as defined in [zip 224](https://github.com/zcash/zips/blob/main/zips/zip-0244.rst).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TransactionId([u8; 32]);

impl From<[u8; 32]> for TransactionId {
    fn from(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl From<TransactionId> for [u8; 32] {
    fn from(h: TransactionId) -> Self {
        h.0
    }
}

impl fmt::Debug for TransactionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "TxHash({:02x}{:02x}{:02x}{:02x}…)",
            self.0[31], self.0[30], self.0[29], self.0[28]
        )
    }
}

impl fmt::Display for TransactionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for &byte in self.0.iter().rev() {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_bytes() {
        let bytes = [0x42; 32];
        let hash = TransactionId::from(bytes);
        assert_eq!(<[u8; 32]>::from(hash), bytes);
    }

    #[test]
    fn display_is_big_endian_hex() {
        let mut bytes = [0u8; 32];
        bytes[31] = 0xFF;
        let hash = TransactionId::from(bytes);
        let display = format!("{hash}");
        assert!(display.starts_with("ff"), "got: {display}");
        assert_eq!(display.len(), 64);
    }
}
