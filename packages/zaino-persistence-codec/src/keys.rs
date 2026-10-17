//! Shared [`PersistentRecord`] records for the key shapes many indexes share.
//!
//! Most indexes key on one of two primitive shapes: a block **height** (an
//! 8-byte big-endian integer) or a 32-byte **hash**. Rather than each index
//! re-deriving that layout, it names the shared record here and gets the bytes —
//! and the [`format_version`](crate::format_version) fingerprint over it — for
//! free.
//!
//! Both are generic over the domain type so a caller pins the *typed* key
//! (`HeightKey<BlockHeight>`, `HashKey<BlockHash>`) while the on-disk layout
//! stays fixed. The bound is the standard numeric/array conversion, so any
//! newtype that already converts to and from `u64` / `[u8; 32]` reuses the
//! record without extra glue.

use crate::layout::{BeU64, Cursor, LayoutAtom, Writer};
use crate::{DecodeError, PersistentRecord, RecordLayout};

/// The on-disk record for a `u64`-valued domain key: 8 bytes, big-endian.
///
/// Reused for any key or value that is exactly a block height.
///
/// Big-endian because a height is a **key**, and a key's byte order is its sort
/// order: a backend that compares keys byte-lexicographically orders
/// big-endian heights numerically, so a cursor walks them in chain order and an
/// in-order append lands at the end of the key space rather than in its middle.
/// The same reason the derive offers
/// [`#[persistent(be)]`](macro@crate::PersistentRecord) — this record is that
/// attribute's shape, named once.
pub struct HeightKey<K>(pub K);

impl<K> RecordLayout for HeightKey<K>
where
    K: Copy + From<u64> + Into<u64>,
{
    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::with_capacity(8);
        BeU64(self.0.into()).encode(&mut writer);
        writer.into_bytes()
    }

    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut cursor = Cursor::new(bytes);
        let raw = BeU64::decode(&mut cursor)?.0;
        cursor.finish()?;
        Ok(Self(K::from(raw)))
    }
}

impl<K> PersistentRecord for HeightKey<K>
where
    K: Copy + From<u64> + Into<u64>,
{
    type Domain = K;

    fn from_domain(domain: &K) -> Self {
        Self(*domain)
    }

    fn into_domain(self) -> Result<K, DecodeError> {
        Ok(self.0)
    }
}

/// The on-disk record for a 32-byte domain key: the bytes verbatim.
///
/// Reused for any key whose domain is a 32-byte hash (block hash, txid, ...).
pub struct HashKey<K>(pub K);

impl<K> RecordLayout for HashKey<K>
where
    K: Copy + From<[u8; 32]> + Into<[u8; 32]>,
{
    fn encode(&self) -> Vec<u8> {
        let bytes: [u8; 32] = self.0.into();
        bytes.to_vec()
    }

    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let array: [u8; 32] = bytes
            .try_into()
            .map_err(|_| DecodeError::Invalid(format!("expected 32 bytes, got {}", bytes.len())))?;
        Ok(Self(K::from(array)))
    }
}

impl<K> PersistentRecord for HashKey<K>
where
    K: Copy + From<[u8; 32]> + Into<[u8; 32]>,
{
    type Domain = K;

    fn from_domain(domain: &K) -> Self {
        Self(*domain)
    }

    fn into_domain(self) -> Result<K, DecodeError> {
        Ok(self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Height(u64);
    impl From<u64> for Height {
        fn from(v: u64) -> Self {
            Self(v)
        }
    }
    impl From<Height> for u64 {
        fn from(v: Height) -> Self {
            v.0
        }
    }

    #[test]
    fn height_key_is_eight_big_endian_bytes() {
        let record = HeightKey::from_domain(&Height(0x0102_0304_0506_0708));
        assert_eq!(record.encode(), vec![1, 2, 3, 4, 5, 6, 7, 8]);
        let back = HeightKey::<Height>::decode(&record.encode())
            .expect("decode")
            .into_domain()
            .expect("into_domain");
        assert_eq!(back, Height(0x0102_0304_0506_0708));
    }

    /// The property the big-endian choice exists for: byte order *is* numeric
    /// order, so a byte-comparing backend walks heights in chain order.
    #[test]
    fn height_key_bytes_sort_in_numeric_order() {
        let heights = [0u64, 1, 2, 255, 256, 257, 65_535, 65_536, u32::MAX.into()];
        let encoded: Vec<Vec<u8>> = heights
            .iter()
            .map(|h| HeightKey::from_domain(&Height(*h)).encode())
            .collect();

        let mut sorted = encoded.clone();
        sorted.sort();
        assert_eq!(
            encoded, sorted,
            "encoded heights must already be in byte-lexicographic order"
        );
    }

    #[test]
    fn hash_key_is_the_bytes_verbatim() {
        let record = HashKey::from_domain(&[9u8; 32]);
        assert_eq!(record.encode(), vec![9u8; 32]);
        assert!(HashKey::<[u8; 32]>::decode(&[0u8; 31]).is_err());
    }
}
