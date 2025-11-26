//! DB stored data wrappers structs.

use crate::{
    read_fixed_le, version, write_fixed_le, CompactSize, FixedEncodedLen, ZainoVersionedSerde,
};

use blake2::{
    digest::{Update, VariableOutput},
    Blake2bVar,
};
use core2::io::{self, Read, Write};

/// A fixed length database entry.
/// This is an important distinction for correct usage of DUP_SORT and DUP_FIXED
/// LMDB database flags.
///
/// Encoded Format:
///
/// ┌─────── byte 0 ───────┬───── byte 1 ─────┬───── T::raw_len() bytes ──────┬─── 32 bytes ────┐
/// │ StoredEntry version  │  Record version  │             Body              │ B2B256 hash     │
/// └──────────────────────┴──────────────────┴───────────────────────────────┴─────────────────┘
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredEntryFixed<T: ZainoVersionedSerde + FixedEncodedLen> {
    /// Inner record
    pub(crate) item: T,
    /// Entry checksum
    pub(crate) checksum: [u8; 32],
}

impl<T: ZainoVersionedSerde + FixedEncodedLen> StoredEntryFixed<T> {
    /// Create a new entry, hashing `key || encoded_item`.
    pub(crate) fn new<K: AsRef<[u8]>>(key: K, item: T) -> Self {
        let body = {
            let mut v = Vec::with_capacity(T::VERSIONED_LEN);
            item.serialize(&mut v).unwrap();
            v
        };
        let checksum = Self::blake2b256(&[key.as_ref(), &body].concat());
        Self { item, checksum }
    }

    /// Verify checksum given the DB key.
    /// Returns `true` if `self.checksum == blake2b256(key || item.serialize())`.
    pub(crate) fn verify<K: AsRef<[u8]>>(&self, key: K) -> bool {
        let body = {
            let mut v = Vec::with_capacity(T::VERSIONED_LEN);
            self.item.serialize(&mut v).unwrap();
            v
        };
        let candidate = Self::blake2b256(&[key.as_ref(), &body].concat());
        candidate == self.checksum
    }

    /// Returns a reference to the inner item.
    pub(crate) fn inner(&self) -> &T {
        &self.item
    }

    /// Computes a BLAKE2b-256 checksum.
    pub(crate) fn blake2b256(data: &[u8]) -> [u8; 32] {
        let mut hasher = Blake2bVar::new(32).expect("Failed to create hasher");
        hasher.update(data);
        let mut output = [0u8; 32];
        hasher
            .finalize_variable(&mut output)
            .expect("Failed to finalize hash");
        output
    }
}

impl<T: ZainoVersionedSerde + FixedEncodedLen> ZainoVersionedSerde for StoredEntryFixed<T> {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        self.item.serialize(&mut *w)?;
        write_fixed_le::<32, _>(&mut *w, &self.checksum)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut body = vec![0u8; T::VERSIONED_LEN];
        r.read_exact(&mut body)?;
        let item = T::deserialize(&body[..])?;

        let checksum = read_fixed_le::<32, _>(r)?;
        Ok(Self { item, checksum })
    }
}

impl<T: ZainoVersionedSerde + FixedEncodedLen> FixedEncodedLen for StoredEntryFixed<T> {
    const ENCODED_LEN: usize = T::VERSIONED_LEN + 32;
}

/// Variable-length database value.
/// Layout (little-endian unless noted):
///
/// ┌────── byte 0 ───────┬─────── CompactSize(len) ─────┬──── 1 byte ────┬── len - 1 bytes ───┬─ 32 bytes ─┐
/// │ StoredEntry version │ (length of item.serialize()) │ Record version │        Body        │    Hash    │
/// └─────────────────────┴──────────────────────────────┴────────────────┴────────────────────┴────────────┘
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredEntryVar<T: ZainoVersionedSerde> {
    /// Inner record
    pub(crate) item: T,
    /// Entry checksum
    pub(crate) checksum: [u8; 32],
}

impl<T: ZainoVersionedSerde> StoredEntryVar<T> {
    /// Create a new entry, hashing `encoded_key || encoded_item`.
    pub(crate) fn new<K: AsRef<[u8]>>(key: K, item: T) -> Self {
        let body = {
            let mut v = Vec::new();
            item.serialize(&mut v).unwrap();
            v
        };
        let checksum = Self::blake2b256(&[key.as_ref(), &body].concat());
        Self { item, checksum }
    }

    /// Verify checksum given the DB key.
    /// Returns `true` if `self.checksum == blake2b256(key || item.serialize())`.
    pub(crate) fn verify<K: AsRef<[u8]>>(&self, key: K) -> bool {
        let mut body = Vec::new();
        self.item.serialize(&mut body).unwrap();
        let candidate = Self::blake2b256(&[key.as_ref(), &body].concat());
        candidate == self.checksum
    }

    /// Returns a reference to the inner item.
    pub(crate) fn inner(&self) -> &T {
        &self.item
    }

    /// Computes a BLAKE2b-256 checksum.
    pub(crate) fn blake2b256(data: &[u8]) -> [u8; 32] {
        let mut hasher = Blake2bVar::new(32).expect("Failed to create hasher");
        hasher.update(data);
        let mut output = [0u8; 32];
        hasher
            .finalize_variable(&mut output)
            .expect("Failed to finalize hash");
        output
    }
}

impl<T: ZainoVersionedSerde> ZainoVersionedSerde for StoredEntryVar<T> {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut body = Vec::new();
        self.item.serialize(&mut body)?;

        CompactSize::write(&mut *w, body.len())?;
        w.write_all(&body)?;
        write_fixed_le::<32, _>(&mut *w, &self.checksum)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let len = CompactSize::read(&mut *r)? as usize;

        let mut body = vec![0u8; len];
        r.read_exact(&mut body)?;
        let item = T::deserialize(&body[..])?;

        let checksum = read_fixed_le::<32, _>(r)?;
        Ok(Self { item, checksum })
    }
}
