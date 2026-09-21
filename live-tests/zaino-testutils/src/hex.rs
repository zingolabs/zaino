//! Hex conversion for payloads that cross the JSON-RPC / gRPC boundary.
//!
//! zaino's gRPC surface hands back raw bytes (`RawTransaction.data`, compact
//! block hashes) while the validator's JSON-RPC surface hands back hex strings
//! for the same objects. Every test that uses the validator as an oracle for
//! something zaino served has to bridge that, so the conversion lives here
//! rather than being re-privately-defined per test file.

use anyhow::{Context, Result};

/// Decode a lowercase-or-uppercase hex string into bytes.
///
/// `label` names what was being decoded, so a malformed payload says which
/// RPC produced it rather than just "invalid digit".
pub fn decode(s: &str, label: &str) -> Result<Vec<u8>> {
    hex::decode(s).with_context(|| format!("{label}: `{s}` is not a hex payload"))
}

/// Encode bytes as a lowercase hex string, the form the validator's JSON-RPC
/// responses use.
pub fn encode(bytes: &[u8]) -> String {
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let bytes = vec![0x00, 0x0f, 0xa5, 0xff];
        assert_eq!(encode(&bytes), "000fa5ff");
        assert_eq!(decode("000fa5ff", "test").unwrap(), bytes);
    }

    #[test]
    fn decode_accepts_uppercase() {
        assert_eq!(decode("A5FF", "test").unwrap(), vec![0xa5, 0xff]);
    }

    #[test]
    fn decode_rejects_odd_length() {
        let err = format!("{:#}", decode("abc", "getrawtransaction").unwrap_err());
        assert!(err.contains("Odd number of digits"), "unexpected: {err}");
        assert!(
            err.contains("getrawtransaction"),
            "error must name the source: {err}"
        );
    }

    #[test]
    fn decode_rejects_non_hex() {
        let err = format!("{:#}", decode("zz", "getrawtransaction").unwrap_err());
        assert!(
            err.contains("getrawtransaction"),
            "error must name the source: {err}"
        );
    }

    /// Multi-byte input used to be sliced at a byte offset inside a char.
    #[test]
    fn decode_reports_non_ascii_rather_than_panicking() {
        let err = format!("{:#}", decode("aéb", "getrawtransaction").unwrap_err());
        assert!(
            err.contains("getrawtransaction"),
            "error must name the source: {err}"
        );
    }
}
