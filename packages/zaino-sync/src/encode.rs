//! Encoding and decoding traits for types that cross the persistence boundary.
//!
//! Types that appear as keys or values in index entries implement
//! [`Encode`] and [`Decode`] to define their byte representation.
//! Index authors never call these directly — the bridge does it
//! mechanically when converting typed entries into
//! [`WriteOp`](crate::traits::WriteOp)s and when loading state back
//! from the backend.
//!
//! This is the serialization single source of truth. If the encoding
//! for `BlockHeight` changes, it changes in one place — not scattered
//! across every index's persist impl.

use crate::primitives::BlockHeight;

/// Serialize a value to its on-disk byte representation.
///
/// Implementations must be deterministic — the same value must always
/// produce the same bytes. This is required for key lookups and for
/// cross-index consistency.
pub trait Encode {
    /// Encode this value into bytes.
    fn encode(&self) -> Vec<u8>;
}

/// Deserialize a value from its on-disk byte representation.
///
/// The inverse of [`Encode`]. Implementations must round-trip:
/// `Decode::decode(&value.encode()) == Ok(value)`.
pub trait Decode: Sized {
    /// Decode a value from bytes.
    fn decode(bytes: &[u8]) -> Result<Self, DecodeError>;
}

/// Errors during decoding.
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    /// The byte slice has the wrong length.
    #[error("invalid length: expected {expected}, got {got}")]
    InvalidLength {
        /// Expected byte count.
        expected: usize,
        /// Actual byte count.
        got: usize,
    },
    /// A generic decode failure.
    #[error("decode failed: {0}")]
    Failed(String),
}

impl Encode for u8 {
    fn encode(&self) -> Vec<u8> {
        vec![*self]
    }
}

impl Decode for u8 {
    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        if bytes.len() != 1 {
            return Err(DecodeError::InvalidLength {
                expected: 1,
                got: bytes.len(),
            });
        }
        Ok(bytes[0])
    }
}

impl Encode for u16 {
    fn encode(&self) -> Vec<u8> {
        self.to_le_bytes().to_vec()
    }
}

impl Decode for u16 {
    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let arr: [u8; 2] = bytes.try_into().map_err(|_| DecodeError::InvalidLength {
            expected: 2,
            got: bytes.len(),
        })?;
        Ok(Self::from_le_bytes(arr))
    }
}

impl Encode for u32 {
    fn encode(&self) -> Vec<u8> {
        self.to_le_bytes().to_vec()
    }
}

impl Decode for u32 {
    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let arr: [u8; 4] = bytes.try_into().map_err(|_| DecodeError::InvalidLength {
            expected: 4,
            got: bytes.len(),
        })?;
        Ok(Self::from_le_bytes(arr))
    }
}

impl Encode for u64 {
    fn encode(&self) -> Vec<u8> {
        self.to_le_bytes().to_vec()
    }
}

impl Decode for u64 {
    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let arr: [u8; 8] = bytes.try_into().map_err(|_| DecodeError::InvalidLength {
            expected: 8,
            got: bytes.len(),
        })?;
        Ok(Self::from_le_bytes(arr))
    }
}

impl Encode for BlockHeight {
    fn encode(&self) -> Vec<u8> {
        self.value().to_le_bytes().to_vec()
    }
}

impl Decode for BlockHeight {
    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let raw = u64::decode(bytes)?;
        Ok(Self::new(raw))
    }
}

impl Encode for &'static [u8] {
    fn encode(&self) -> Vec<u8> {
        self.to_vec()
    }
}

impl Encode for &'static str {
    fn encode(&self) -> Vec<u8> {
        self.as_bytes().to_vec()
    }
}
