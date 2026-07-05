//! Checksummed database entry wrappers (fixed and variable length)
//!
//! This file defines small wrapper types used by concrete DB versions for storing values in
//! LMDB with an **integrity checksum**.
//!
//! Each wrapper stores:
//! - the inner *versioned* record `T: ZainoVersionedSerde`, and
//! - a BLAKE2b-256 checksum computed over `key || encoded_item`.
//!
//! The checksum is intended to:
//! - detect corruption or partial writes,
//! - detect accidental key/value mismatches (e.g., writing under the wrong key encoding),
//! - and provide a cheap integrity check during migrations or debugging.
//!
//! ## Integrity model (scope)
//!
//! The checksum is a **corruption and correctness** signal, not a cryptographic authentication
//! mechanism. It helps detect accidental corruption, partial writes, or key/value mismatches, but
//! it does not provide authenticity against a malicious database writer, this must be ensured in
//! actual database implementations by validating block data on startup and on block writes.
//!
//! # Two wrapper forms
//!
//! - [`StoredEntryFixed<T>`] for fixed-length values:
//!   - requires `T: FixedEncodedLen` so that the total encoded value length is constant.
//!   - important when LMDB uses `DUP_SORT` and/or `DUP_FIXED` flags where record sizing matters.
//!
//! - [`StoredEntryVar<T>`] for variable-length values:
//!   - prefixes the serialized record with a CompactSize length so decoding is bounded and safe.
//!
//! Both wrappers are themselves versioned (`ZainoVersionedSerde`), which means their outer layout can
//! evolve in a controlled way if required.
//!
//! # Encoding contract (conceptual)
//!
//! `StoredEntryFixed<T>` encodes as:
//! - StoredEntry version tag
//! - `T::serialize()` bytes (which include `T`'s own record version tag)
//! - 32-byte checksum
//!
//! `StoredEntryVar<T>` encodes as:
//! - StoredEntry version tag
//! - CompactSize(length of `T::serialize()` bytes)
//! - `T::serialize()` bytes
//! - 32-byte checksum
//!
//! # Usage guidelines
//!
//! - Always compute the checksum using the **exact bytes** used as the DB key (i.e. the encoded key).
//! - On read, verify the checksum before trusting decoded contents.
//! - Treat checksum mismatch as a corruption/incompatibility signal:
//!   - return a hard error,
//!   - or trigger a rebuild path, depending on the calling context.
//!
//! # Development: when to pick fixed vs var
//!
//! - Use `StoredEntryFixed<T>` when:
//!   - `T` has a stable, fixed-size encoding and you want predictable sizing, or
//!   - the LMDB table relies on fixed-size duplicates.
//!
//! - Use `StoredEntryVar<T>` when:
//!   - `T` naturally contains variable-length payloads (vectors, scripts, etc.), or
//!   - the value size may grow over time and you want to avoid schema churn.
//!
//! If you change the wrapper layout itself, bump the wrapper’s `ZainoVersionedSerde::VERSION` and
//! maintain a decode path (or bump the DB major version and migrate).

use crate::{
    read_fixed_le, read_u8, version, write_fixed_le, CompactSize, FixedEncodedLen,
    ZainoVersionedSerde,
};

use blake2::{
    digest::{Update, VariableOutput},
    Blake2bVar,
};
use corez::io::{self, Read, Write};

/// Fixed-length checksummed database value wrapper.
///
/// This wrapper is designed for LMDB tables that rely on fixed-size value records, including those
/// configured with `DUP_SORT` and/or `DUP_FIXED`.
///
/// The wrapper stores:
/// - a versioned record `T` (encoded via [`ZainoVersionedSerde`]), and
/// - a 32-byte BLAKE2b-256 checksum computed over `encoded_key || encoded_item`.
///
/// ## Invariants
/// - `T` must have a fixed encoded length (including its own version tag), enforced by
///   [`FixedEncodedLen`].
/// - The checksum must be computed using the **exact key bytes** used in LMDB for this entry.
/// - On read, callers should verify the checksum before trusting decoded contents.
///
/// ## Encoded format (conceptual)
///
/// ┌─────── byte 0 ───────┬────────────── T::serialize() bytes ──────────────┬─── 32 bytes ────┐
/// │ StoredEntry version  │ (includes T's own record version tag + body)     │ B2B256 checksum │
/// └──────────────────────┴──────────────────────────────────────────────────┴─────────────────┘
///
/// Where the checksum is:
/// `blake2b256(encoded_key || encoded_item_bytes)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredEntryFixed<T: ZainoVersionedSerde + FixedEncodedLen> {
    /// The inner record stored in this entry.
    pub(crate) item: T,

    /// BLAKE2b-256 checksum of `encoded_key || encoded_item_bytes`.
    pub(crate) checksum: [u8; 32],
}

impl<T: ZainoVersionedSerde + FixedEncodedLen> StoredEntryFixed<T> {
    /// Constructs a new checksummed fixed-length entry for `item` under `key`.
    ///
    /// The checksum is computed over:
    ///
    /// `encoded_key || item.serialize()`
    ///
    /// This constructor may only be used when the latest version of `T` is still
    /// fixed-length. If the latest version of `T` has become variable-length,
    /// callers must use the variable-length stored-entry wrapper for new writes.
    ///
    /// # Panics
    ///
    /// Panics if `T::serialize()` fails. Existing code already treated this path
    /// as infallible. If desired, this can be made fallible later without changing
    /// the on-disk format.
    pub(crate) fn new<K: AsRef<[u8]>>(key: K, item: T) -> Self {
        let body = {
            let len = T::latest_versioned_len().unwrap_or(0);
            let mut v = Vec::with_capacity(len);
            item.serialize(&mut v).unwrap();
            v
        };

        let checksum = Self::blake2b256(&[key.as_ref(), &body].concat());
        Self { item, checksum }
    }

    /// Verifies the checksum for this entry under `key`.
    ///
    /// Verification tries every supported historical encoding from latest down
    /// to v1. This is required because the decoded in-memory item no longer
    /// remembers which exact version produced the stored checksum.
    pub(crate) fn verify<K: AsRef<[u8]>>(&self, key: K) -> bool {
        let mut v = T::VERSION;

        loop {
            if let Ok(item_bytes) = self.item.to_bytes_with_version(v) {
                let candidate = Self::blake2b256(&[key.as_ref(), &item_bytes].concat());
                if candidate == self.checksum {
                    return true;
                }
            }

            if v == 1 {
                break;
            }

            v = v.saturating_sub(1);
        }

        false
    }

    /// Returns a reference to the inner decoded record.
    pub(crate) fn inner(&self) -> &T {
        &self.item
    }

    /// Computes a BLAKE2b-256 checksum over `data`.
    pub(crate) fn blake2b256(data: &[u8]) -> [u8; 32] {
        let mut hasher = Blake2bVar::new(32).expect("Failed to create hasher");
        hasher.update(data);

        let mut output = [0u8; 32];
        hasher
            .finalize_variable(&mut output)
            .expect("Failed to finalize hash");

        output
    }

    /// Builds serialized stored-entry bytes using a specific inner item version.
    ///
    /// This is used for tests and historical fixture construction.
    ///
    /// The selected `item_version` must be fixed-length according to
    /// `T::versioned_len(item_version)`.
    #[cfg(test)]
    pub(crate) fn to_bytes_with_item_version<K: AsRef<[u8]>>(
        key: K,
        item: &T,
        item_version: u8,
    ) -> io::Result<Vec<u8>> {
        let item_bytes = item.to_bytes_with_version(item_version)?;

        let expected_len = T::versioned_len(item_version).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "requested item version is not fixed-length",
            )
        })?;

        if item_bytes.len() != expected_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "encoded fixed-length item has an unexpected length",
            ));
        }

        let checksum = Self::blake2b256(&[key.as_ref(), &item_bytes].concat());

        let mut stored_entry_bytes = Vec::with_capacity(1 + expected_len + 32);
        stored_entry_bytes.push(Self::VERSION);
        stored_entry_bytes.extend_from_slice(&item_bytes);
        stored_entry_bytes.extend_from_slice(&checksum);

        Ok(stored_entry_bytes)
    }
}

impl<T: ZainoVersionedSerde + FixedEncodedLen> ZainoVersionedSerde for StoredEntryFixed<T> {
    const VERSION: u8 = version::V1;

    /// Encodes the latest fixed stored-entry layout.
    fn encode_latest<W: Write>(&self, w: &mut W) -> io::Result<()> {
        Self::encode_v1(self, w)
    }

    /// Decodes the latest fixed stored-entry layout.
    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    /// Encodes a fixed stored entry.
    ///
    /// The inner item is serialized with its own latest version tag and body,
    /// followed by the 32-byte checksum.
    ///
    /// This method requires the latest version of `T` to still be fixed-length.
    fn encode_v1<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let expected_len = T::latest_versioned_len().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "latest item version is not fixed-length",
            )
        })?;

        let mut item_bytes = Vec::with_capacity(expected_len);
        self.item.serialize(&mut item_bytes)?;

        if item_bytes.len() != expected_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "encoded fixed-length item has an unexpected length",
            ));
        }

        w.write_all(&item_bytes)?;
        write_fixed_le::<32, _>(&mut *w, &self.checksum)
    }

    /// Decodes a fixed stored entry.
    ///
    /// This method reads the inner item version tag first, then uses
    /// `T::encoded_len(version)` to determine how many body bytes to read.
    ///
    /// This is what keeps old fixed-length rows readable after the latest `T`
    /// changes size or becomes variable-length.
    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let item_version = read_u8(&mut *r)?;

        let body_len = T::encoded_len(item_version).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "stored item version is not fixed-length or is unsupported",
            )
        })?;

        let mut item_bytes = Vec::with_capacity(1 + body_len);
        item_bytes.push(item_version);
        item_bytes.resize(1 + body_len, 0);

        r.read_exact(&mut item_bytes[1..])?;

        let item = T::deserialize(&item_bytes[..])?;
        let checksum = read_fixed_le::<32, _>(r)?;

        Ok(Self { item, checksum })
    }
}

/// Fixed stored entries are only fixed-length when the latest inner item version
/// is fixed-length.
impl<T: ZainoVersionedSerde + FixedEncodedLen> FixedEncodedLen for StoredEntryFixed<T> {
    /// Returns the fixed encoded body length for a stored fixed entry.
    ///
    /// `StoredEntryFixed` v1 consists of:
    /// - the latest fixed-length versioned encoding of the inner item `T`
    /// - a 32-byte checksum
    ///
    /// If the latest version of `T` is not fixed-length, this returns `None`
    /// because `StoredEntryFixed<T>` cannot have a fixed latest encoded length.
    fn encoded_len(version: u8) -> Option<usize> {
        match version {
            version::V1 => T::latest_versioned_len().ok().map(|item_len| item_len + 32),
            _ => None,
        }
    }
}

/// Variable-length checksummed database value wrapper.
///
/// This wrapper is used for values whose serialized representation is not fixed-size. It stores:
/// - a versioned record `T` (encoded via [`ZainoVersionedSerde`]),
/// - a CompactSize length prefix for the serialized record,
/// - and a 32-byte BLAKE2b-256 checksum computed over `encoded_key || encoded_item`.
///
/// The length prefix allows decoding to be bounded and avoids reading untrusted trailing bytes.
///
/// ## Encoded format (conceptual)
///
/// ┌────── byte 0 ───────┬────── CompactSize(len) ──────┬────── len bytes ──────┬─ 32 bytes ─┐
/// │ StoredEntry version │ len = item.serialize().len() │ T::serialize() bytes  │  checksum  │
/// └─────────────────────┴──────────────────────────────┴───────────────────────┴────────────┘
///
/// Where the checksum is:
/// `blake2b256(encoded_key || encoded_item_bytes)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredEntryVar<T: ZainoVersionedSerde> {
    /// The inner record stored in this entry.
    pub(crate) item: T,
    /// BLAKE2b-256 checksum of `encoded_key || encoded_item_bytes`.
    pub(crate) checksum: [u8; 32],
}

impl<T: ZainoVersionedSerde> StoredEntryVar<T> {
    /// Constructs a new checksummed entry for `item` under `key`.
    ///
    /// The checksum is computed as:
    /// `blake2b256(encoded_key || item.serialize())`.
    ///
    /// # Key requirements
    /// `key` must be the exact byte encoding used as the LMDB key for this record.
    pub(crate) fn new<K: AsRef<[u8]>>(key: K, item: T) -> Self {
        let body = {
            let mut v = Vec::new();
            item.serialize(&mut v).unwrap();
            v
        };
        let checksum = Self::blake2b256(&[key.as_ref(), &body].concat());
        Self { item, checksum }
    }

    /// Verifies the checksum for this entry under `key`.
    ///
    /// Returns `true` if and only if:
    /// `self.checksum == blake2b256(encoded_key || item.serialize())`.
    ///
    /// # Key requirements
    /// `key` must be the exact byte encoding used as the LMDB key for this record.
    pub(crate) fn verify<K: AsRef<[u8]>>(&self, key: K) -> bool {
        // Iterate from latest (T::VERSION) down to 1 (inclusive).
        let mut v = T::VERSION;
        loop {
            // Try to obtain the encoded bytes for this candidate version (tag + body).
            match self.item.to_bytes_with_version(v) {
                Ok(item_bytes) => {
                    // Compute the candidate checksum over (encoded_key || item_bytes).
                    let candidate = Self::blake2b256(&[key.as_ref(), &item_bytes].concat());
                    if candidate == self.checksum {
                        return true;
                    }
                }
                Err(_) => {
                    // This version not supported by the type's encoder; try older version.
                }
            }

            if v == 1 {
                break;
            }
            v = v.saturating_sub(1);
        }

        false
    }

    /// Returns a reference to the inner record.
    pub(crate) fn inner(&self) -> &T {
        &self.item
    }

    /// Computes a BLAKE2b-256 checksum over `data`.
    pub(crate) fn blake2b256(data: &[u8]) -> [u8; 32] {
        let mut hasher = Blake2bVar::new(32).expect("Failed to create hasher");
        hasher.update(data);
        let mut output = [0u8; 32];
        hasher
            .finalize_variable(&mut output)
            .expect("Failed to finalize hash");
        output
    }

    /// Builds serialized stored-entry bytes using a specific inner item version.
    ///
    /// This is required when writing historical database values whose inner item must be encoded
    /// with an older version than `T::VERSION`.
    ///
    /// The returned bytes are:
    /// - StoredEntryVar version tag
    /// - CompactSize length of the inner item bytes
    /// - inner item bytes encoded with `item_version`
    /// - checksum over `encoded_key || inner_item_bytes`
    ///
    /// This method returns serialized bytes directly because `StoredEntryVar<T>` does not store
    /// the inner item version. Constructing `Self` alone would lose the requested item version
    /// before the value is written.
    #[cfg(test)]
    pub(crate) fn to_bytes_with_item_version<K: AsRef<[u8]>>(
        key: K,
        item: &T,
        item_version: u8,
    ) -> io::Result<Vec<u8>> {
        let item_bytes = item.to_bytes_with_version(item_version)?;
        let checksum = Self::blake2b256(&[key.as_ref(), &item_bytes].concat());

        let mut stored_entry_bytes = Vec::new();
        stored_entry_bytes.push(Self::VERSION);
        CompactSize::write(&mut stored_entry_bytes, item_bytes.len())?;
        stored_entry_bytes.extend_from_slice(&item_bytes);
        write_fixed_le::<32, _>(&mut stored_entry_bytes, &checksum)?;

        Ok(stored_entry_bytes)
    }
}

/// Versioned on-disk encoding for variable-length checksummed entries.
///
/// Body layout (after the `StoredEntryVar` version tag):
/// 1. CompactSize `len` (the length of `T::serialize()` bytes)
/// 2. `len` bytes of `T::serialize()` (includes `T`’s own version tag and body)
/// 3. 32-byte checksum
///
/// Implementations must ensure the length prefix matches the exact serialized record bytes written,
/// otherwise decoding will fail or misalign.
impl<T: ZainoVersionedSerde> ZainoVersionedSerde for StoredEntryVar<T> {
    const VERSION: u8 = version::V1;

    fn encode_latest<W: Write>(&self, w: &mut W) -> io::Result<()> {
        Self::encode_v1(self, w)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn encode_v1<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut body = Vec::new();
        self.item.serialize(&mut body)?;

        CompactSize::write(&mut *w, body.len())?;
        w.write_all(&body)?;
        write_fixed_le::<32, _>(&mut *w, &self.checksum)
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

#[cfg(test)]
mod tests {
    use crate::{read_u32_be, read_u32_le, write_u32_be, write_u32_le};

    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct TestInner {
        pub x: u32,
    }

    impl ZainoVersionedSerde for TestInner {
        const VERSION: u8 = version::V2;

        fn encode_latest<W: Write>(&self, w: &mut W) -> io::Result<()> {
            self.encode_v2(w)
        }

        fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
            Self::decode_v2(r)
        }

        /// v1 body:
        /// - 4 bytes: `x` as little-endian `u32`
        fn encode_v1<W: Write>(&self, w: &mut W) -> io::Result<()> {
            write_u32_le(w, self.x)
        }

        /// v2 body:
        /// - 4 bytes: `x` as big-endian `u32`
        /// - 4 bytes: duplicate/check field as big-endian `u32`
        ///
        /// This intentionally makes v2 a different fixed length from v1 so the
        /// `StoredEntryFixed` decoder must use the version tag to select the
        /// correct body length.
        fn encode_v2<W: Write>(&self, w: &mut W) -> io::Result<()> {
            write_u32_be(&mut *w, self.x)?;
            write_u32_be(&mut *w, !self.x)
        }

        fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
            let x = read_u32_le(r)?;
            Ok(TestInner { x })
        }

        fn decode_v2<R: Read>(r: &mut R) -> io::Result<Self> {
            let x = read_u32_be(&mut *r)?;
            let check = read_u32_be(&mut *r)?;

            if check != !x {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid TestInner v2 check field",
                ));
            }

            Ok(TestInner { x })
        }
    }

    impl FixedEncodedLen for TestInner {
        /// Historical fixed body lengths, excluding the version tag.
        ///
        /// v1 is 4 bytes.
        /// v2 is 8 bytes.
        fn encoded_len(version: u8) -> Option<usize> {
            match version {
                version::V1 => Some(4),
                version::V2 => Some(8),
                _ => None,
            }
        }
    }

    fn key_bytes() -> Vec<u8> {
        b"test-key".to_vec()
    }

    #[test]
    fn test_inner_versions_have_different_lengths() {
        let inner = TestInner { x: 0x1122_3344 };

        let v1 = inner
            .to_bytes_with_version(version::V1)
            .expect("inner v1 bytes");
        let v2 = inner
            .to_bytes_with_version(version::V2)
            .expect("inner v2 bytes");

        assert_eq!(v1.len(), 1 + 4);
        assert_eq!(v2.len(), 1 + 8);
        assert_ne!(v1.len(), v2.len());

        assert_eq!(TestInner::versioned_len(version::V1), Some(1 + 4));
        assert_eq!(TestInner::versioned_len(version::V2), Some(1 + 8));
    }

    #[test]
    fn stored_entry_fixed_roundtrip_latest() {
        let inner = TestInner { x: 0x1122_3344 };
        let key = key_bytes();

        let wrapper = StoredEntryFixed::new(&key, inner.clone());

        assert!(wrapper.verify(&key), "wrapper verify latest should succeed");

        let bytes = wrapper.to_bytes().expect("wrapper to_bytes");
        let parsed = StoredEntryFixed::<TestInner>::from_bytes(&bytes).expect("from_bytes");

        assert_eq!(parsed.item, inner);
        assert_eq!(parsed.checksum, wrapper.checksum);
        assert!(parsed.verify(&key));
    }

    #[test]
    fn stored_entry_fixed_decodes_historical_v1_with_shorter_length() {
        let inner = TestInner { x: 0xAABB_CCDD };
        let key = key_bytes();

        let item_bytes_v1 = inner
            .to_bytes_with_version(version::V1)
            .expect("inner v1 bytes");

        assert_eq!(
            item_bytes_v1.len(),
            TestInner::versioned_len(version::V1).unwrap()
        );
        assert_ne!(
            item_bytes_v1.len(),
            TestInner::latest_versioned_len().unwrap()
        );

        let mut digest_input = Vec::with_capacity(key.len() + item_bytes_v1.len());
        digest_input.extend_from_slice(&key);
        digest_input.extend_from_slice(&item_bytes_v1);

        let checksum = StoredEntryFixed::<TestInner>::blake2b256(&digest_input);

        let mut raw = Vec::new();
        raw.push(StoredEntryFixed::<TestInner>::VERSION);
        raw.extend_from_slice(&item_bytes_v1);
        raw.extend_from_slice(&checksum);

        let parsed = StoredEntryFixed::<TestInner>::from_bytes(&raw)
            .expect("fixed wrapper should decode historical shorter v1 item");

        assert_eq!(parsed.item, inner);
        assert_eq!(parsed.checksum, checksum);
        assert!(parsed.verify(&key));
    }

    #[test]
    fn stored_entry_fixed_decodes_latest_v2_with_longer_length() {
        let inner = TestInner { x: 0x5566_7788 };
        let key = key_bytes();

        let item_bytes_v2 = inner
            .to_bytes_with_version(version::V2)
            .expect("inner v2 bytes");

        assert_eq!(
            item_bytes_v2.len(),
            TestInner::versioned_len(version::V2).unwrap()
        );

        let checksum = StoredEntryFixed::<TestInner>::blake2b256(
            &[key.as_slice(), item_bytes_v2.as_slice()].concat(),
        );

        let mut raw = Vec::new();
        raw.push(StoredEntryFixed::<TestInner>::VERSION);
        raw.extend_from_slice(&item_bytes_v2);
        raw.extend_from_slice(&checksum);

        let parsed = StoredEntryFixed::<TestInner>::from_bytes(&raw)
            .expect("fixed wrapper should decode latest longer v2 item");

        assert_eq!(parsed.item, inner);
        assert_eq!(parsed.checksum, checksum);
        assert!(parsed.verify(&key));
    }

    #[test]
    fn stored_entry_fixed_verify_tamper() {
        let inner = TestInner { x: 0x0102_0304 };
        let key = key_bytes();

        let mut wrapper = StoredEntryFixed::new(&key, inner.clone());
        assert!(wrapper.verify(&key));

        wrapper.checksum[0] ^= 0xff;
        assert!(
            !wrapper.verify(&key),
            "verify should fail with tampered checksum"
        );

        wrapper = StoredEntryFixed::new(&key, inner.clone());
        assert!(wrapper.verify(&key));

        let wrong_key = b"other-key".to_vec();
        assert!(
            !wrapper.verify(&wrong_key),
            "verify should fail with wrong key"
        );
    }

    #[test]
    fn stored_entry_var_roundtrip_latest() {
        let inner = TestInner { x: 0x5566_7788 };
        let key = key_bytes();

        let wrapper = StoredEntryVar::new(&key, inner.clone());

        assert!(
            wrapper.verify(&key),
            "var wrapper verify latest should succeed"
        );

        let bytes = wrapper.to_bytes().expect("var to_bytes");
        let parsed = StoredEntryVar::<TestInner>::from_bytes(&bytes).expect("var from_bytes");

        assert_eq!(parsed.item, inner);
        assert_eq!(parsed.checksum, wrapper.checksum);
        assert!(parsed.verify(&key));
    }

    #[test]
    fn stored_entry_var_verify_old_v1() {
        let inner = TestInner { x: 0xDEAD_BEEF };
        let key = key_bytes();

        let item_bytes_v1 = inner
            .to_bytes_with_version(version::V1)
            .expect("inner v1 bytes");

        let mut digest_input = Vec::with_capacity(key.len() + item_bytes_v1.len());
        digest_input.extend_from_slice(&key);
        digest_input.extend_from_slice(&item_bytes_v1);

        let checksum = StoredEntryVar::<TestInner>::blake2b256(&digest_input);

        let mut raw = Vec::new();
        raw.push(StoredEntryVar::<TestInner>::VERSION);
        CompactSize::write(&mut raw, item_bytes_v1.len()).expect("write compactsize");
        raw.extend_from_slice(&item_bytes_v1);
        write_fixed_le::<32, _>(&mut raw, &checksum).expect("write checksum");

        let parsed = StoredEntryVar::<TestInner>::from_bytes(&raw).expect("var from_bytes v1");

        assert_eq!(parsed.item, inner);
        assert_eq!(parsed.checksum, checksum);
        assert!(parsed.verify(&key));
    }

    #[test]
    fn stored_entry_var_verify_tamper() {
        let inner = TestInner { x: 0xCAFEBABE };
        let key = key_bytes();

        let mut wrapper = StoredEntryVar::new(&key, inner);
        assert!(wrapper.verify(&key));

        wrapper.checksum[31] ^= 0xff;
        assert!(!wrapper.verify(&key));

        let wrapper = StoredEntryVar::new(&key, TestInner { x: 0xCAFEBABE });
        let wrong_key = b"bad-key".to_vec();

        assert!(!wrapper.verify(&wrong_key));
    }
}
