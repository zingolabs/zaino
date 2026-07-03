//! Encoding trait for types that cross the persistence boundary.
//!
//! Types that appear as keys or values in index entries implement
//! [`Encode`] to define their byte representation. Index authors never
//! call `encode` directly — the bridge does it mechanically when
//! converting typed entries into [`WriteOp`](crate::traits::WriteOp)s.
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

impl Encode for u8 {
    fn encode(&self) -> Vec<u8> {
        vec![*self]
    }
}

impl Encode for u16 {
    fn encode(&self) -> Vec<u8> {
        self.to_le_bytes().to_vec()
    }
}

impl Encode for u32 {
    fn encode(&self) -> Vec<u8> {
        self.to_le_bytes().to_vec()
    }
}

impl Encode for u64 {
    fn encode(&self) -> Vec<u8> {
        self.to_le_bytes().to_vec()
    }
}

impl Encode for BlockHeight {
    fn encode(&self) -> Vec<u8> {
        self.value().to_le_bytes().to_vec()
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
