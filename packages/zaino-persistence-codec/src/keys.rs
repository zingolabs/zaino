//! Shared [`PersistentRecord`] records for the key shapes many indexes share.
//!
//! Most indexes key on one of two primitive shapes: a block **height** (an
//! 8-byte little-endian integer) or a 32-byte **hash**. Rather than each index
//! re-deriving that layout, it names the shared record here and gets the bytes —
//! and the [`format_version`](crate::format_version) fingerprint over it — for
//! free.
//!
//! Both are generic over the domain type so a caller pins the *typed* key
//! (`HeightKey<BlockHeight>`, `HashKey<BlockHash>`) while the on-disk layout
//! stays fixed. The bound is the standard numeric/array conversion, so any
//! newtype that already converts to and from `u64` / `[u8; 32]` reuses the
//! record without extra glue.

use crate::layout::{Cursor, Writer};
use crate::{DecodeError, PersistentRecord, RecordLayout};

/// The on-disk record for a `u64`-valued domain key: 8 bytes, little-endian.
///
/// Reused for any key or value that is exactly a block height.
pub struct HeightKey<K>(pub K);

impl<K> RecordLayout for HeightKey<K>
where
    K: Copy + From<u64> + Into<u64>,
{
    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::with_capacity(8);
        writer.u64(self.0.into());
        writer.into_bytes()
    }

    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut cursor = Cursor::new(bytes);
        let raw = cursor.u64()?;
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
    fn height_key_is_eight_little_endian_bytes() {
        let record = HeightKey::from_domain(&Height(0x0102_0304_0506_0708));
        assert_eq!(record.encode(), vec![8, 7, 6, 5, 4, 3, 2, 1]);
        let back = HeightKey::<Height>::decode(&record.encode())
            .expect("decode")
            .into_domain()
            .expect("into_domain");
        assert_eq!(back, Height(0x0102_0304_0506_0708));
    }

    #[test]
    fn hash_key_is_the_bytes_verbatim() {
        let record = HashKey::from_domain(&[9u8; 32]);
        assert_eq!(record.encode(), vec![9u8; 32]);
        assert!(HashKey::<[u8; 32]>::decode(&[0u8; 31]).is_err());
    }
}
