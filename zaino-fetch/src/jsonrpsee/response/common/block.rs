//! Common types related to Zcash blocks.

use core::fmt;
use std::str::FromStr;

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

/// The identifier for a Zcash block.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockHash(pub [u8; 32]);

impl BlockHash {
    /// All-zero hash.
    pub const ZERO: Self = Self([0u8; 32]);

    /// Construct from raw bytes.
    #[inline]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Borrow raw bytes.
    #[inline]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Fallible constructor from any slice, expects 32 bytes.
    pub fn try_from_slice(bytes: &[u8]) -> Result<Self, ParseBlockHashError> {
        if bytes.len() != 32 {
            return Err(ParseBlockHashError::WrongLength(bytes.len()));
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(bytes);
        Ok(Self(out))
    }

    /// Parse from hex. Accepts lowercase/uppercase, with optional `0x`.
    pub fn from_hex_str(s: &str) -> Result<Self, ParseBlockHashError> {
        let s = s.strip_prefix("0x").unwrap_or(s);
        if s.len() != 64 {
            return Err(ParseBlockHashError::WrongLength(s.len()));
        }
        let mut out = [0u8; 32];
        hex_to_fixed(&mut out, s.as_bytes()).map_err(|_| ParseBlockHashError::InvalidHex)?;
        Ok(Self(out))
    }

    /// Lowercase hex, RPC form. No `0x` prefix.
    pub fn to_hex(&self) -> String {
        hex_lower(&self.0)
    }
}

impl fmt::Display for BlockHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for BlockHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Same as in RPCs
        write!(f, "BlockHash({})", self.to_hex())
    }
}

impl Default for BlockHash {
    fn default() -> Self {
        Self::ZERO
    }
}

impl From<[u8; 32]> for BlockHash {
    fn from(b: [u8; 32]) -> Self {
        Self(b)
    }
}
impl AsRef<[u8]> for BlockHash {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl FromStr for BlockHash {
    type Err = ParseBlockHashError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_hex_str(s)
    }
}

// Lowercase
impl Serialize for BlockHash {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}
impl<'de> Deserialize<'de> for BlockHash {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        Self::from_hex_str(s.trim()).map_err(de::Error::custom)
    }
}

/// Error parsing a block hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseBlockHashError {
    /// Length is not 64 hex chars.
    WrongLength(usize),

    /// Invalid hex.
    InvalidHex,
}

impl fmt::Display for ParseBlockHashError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            ParseBlockHashError::WrongLength(n) => {
                write!(f, "block hash must be 64 hex chars, got {n}")
            }
            ParseBlockHashError::InvalidHex => write!(f, "invalid hex in block hash"),
        }
    }
}

// Helpers
fn hex_lower(bytes: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for &b in bytes.iter() {
        use core::fmt::Write;
        let _ = write!(&mut s, "{:02x}", b);
    }
    s
}
fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(10 + (b - b'a')),
        b'A'..=b'F' => Some(10 + (b - b'A')),
        _ => None,
    }
}
fn hex_to_fixed(out: &mut [u8; 32], s: &[u8]) -> Result<(), ()> {
    if s.len() != 64 {
        return Err(());
    }
    for i in 0..32 {
        let hi = hex_nibble(s[2 * i]).ok_or(())?;
        let lo = hex_nibble(s[2 * i + 1]).ok_or(())?;
        out[i] = (hi << 4) | lo;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_display_parse() {
        // bytes 00..1f
        let mut b = [0u8; 32];
        for (i, x) in b.iter_mut().enumerate() {
            *x = i as u8;
        }
        let h = BlockHash::from(b);

        let s = h.to_string();
        assert_eq!(s.len(), 64);
        // Re-parse and match
        let back: BlockHash = s.parse().unwrap();
        assert_eq!(h, back);
        assert_eq!(format!("{:?}", h), format!("BlockHash({s})"));
    }

    #[test]
    fn accepts_uppercase_and_0x() {
        let upper = "0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF";
        let h = BlockHash::from_hex_str(upper).unwrap();
        assert_eq!(
            h.to_string(),
            "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
        );
    }

    #[test]
    fn wrong_length_and_invalid_hex() {
        match BlockHash::from_hex_str("abcd") {
            Err(ParseBlockHashError::WrongLength(4)) => {}
            other => panic!("expected WrongLength, got {other:?}"),
        }
        match BlockHash::from_hex_str(
            "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
        ) {
            Err(ParseBlockHashError::InvalidHex) => {}
            other => panic!("expected InvalidHex, got {other:?}"),
        }
    }

    #[test]
    fn serde_json_roundtrip() {
        let hex = "1234abcd00000000000000000000000000000000000000000000000000000000";
        // de
        let h: BlockHash = serde_json::from_str(&format!(r#""{hex}""#)).unwrap();
        assert_eq!(h.to_string(), hex);
        // ser
        let j = serde_json::to_string(&h).unwrap();
        assert_eq!(j, format!(r#""{hex}""#));
    }

    #[test]
    fn try_from_slice() {
        let data = [0xAB; 32];
        let h = BlockHash::try_from_slice(&data).unwrap();
        assert_eq!(h.as_bytes(), &data);

        let err = BlockHash::try_from_slice(&data[..31]).unwrap_err();
        assert!(matches!(err, ParseBlockHashError::WrongLength(31)));
    }

    #[test]
    fn zero_const() {
        let z = BlockHash::ZERO;
        assert_eq!(
            z.to_string(),
            "0000000000000000000000000000000000000000000000000000000000000000"
        );
    }
}
