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
    read_fixed_le, version, write_fixed_le, CompactSize, FixedEncodedLen, ZainoVersionedSerde,
};

use blake2::{
    digest::{Update, VariableOutput},
    Blake2bVar,
};
use corez::io::{self, Read, Write};

/// Length in bytes of the BLAKE2b-256 checksum trailing every stored entry.
const CHECKSUM_LEN: usize = 32;

/// BLAKE2b-256 over `key ‖ body`, streamed into the hasher — no concatenation
/// allocation. This is the checksum every stored entry carries; `key` must be
/// the exact byte encoding used as the LMDB key for the record.
pub(crate) fn keyed_checksum(key: &[u8], body: &[u8]) -> [u8; 32] {
    let mut hasher = Blake2bVar::new(CHECKSUM_LEN).expect("BLAKE2b-256 output length is valid");
    hasher.update(key);
    hasher.update(body);
    let mut output = [0u8; CHECKSUM_LEN];
    hasher
        .finalize_variable(&mut output)
        .expect("BLAKE2b-256 output length is valid");
    output
}

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
    /// Constructs a new checksummed entry for `item` under `key`.
    ///
    /// The checksum is computed as:
    /// `blake2b256(encoded_key || item.serialize())`.
    ///
    /// # Key requirements
    /// `key` must be the exact byte encoding used as the LMDB key for this record. If the caller
    /// hashes a different key encoding than what is used for storage, verification will fail.
    pub(crate) fn new<K: AsRef<[u8]>>(key: K, item: T) -> Self {
        let body = {
            let mut v = Vec::with_capacity(T::VERSIONED_LEN);
            item.serialize(&mut v).unwrap();
            v
        };
        let checksum = keyed_checksum(key.as_ref(), &body);
        Self { item, checksum }
    }

    /// Encodes the complete stored-entry wire bytes for `item` under `key` in a single
    /// pass: wrapper version tag, item bytes, and the checksum over `key ‖ item bytes`.
    ///
    /// This is the write-path counterpart of `from_bytes`. Prefer it over
    /// `Self::new(key, item).to_bytes()`, which serializes the item twice (once for the
    /// checksum, once for the wire bytes).
    pub(crate) fn encode<K: AsRef<[u8]>>(key: K, item: &T) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::with_capacity(1 + T::VERSIONED_LEN + CHECKSUM_LEN);
        bytes.push(Self::VERSION);
        item.serialize(&mut bytes)?;
        if bytes.len() != 1 + T::VERSIONED_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "encoded fixed-length item has an unexpected length",
            ));
        }
        let checksum = keyed_checksum(key.as_ref(), &bytes[1..]);
        bytes.extend_from_slice(&checksum);
        Ok(bytes)
    }

    /// Verifies the checksum of raw stored-entry bytes under `key` without decoding or
    /// re-encoding the item: the checksum is defined over `key ‖ item bytes`, and both
    /// already sit in `raw` exactly as written.
    ///
    /// Version-agnostic by construction — the checksum binds whatever encoding version
    /// the writer used — and therefore both cheaper and stricter than [`Self::verify`],
    /// which re-encodes the decoded item per candidate version.
    pub(crate) fn verify_stored<K: AsRef<[u8]>>(key: K, raw: &[u8]) -> bool {
        if raw.len() != 1 + T::VERSIONED_LEN + CHECKSUM_LEN {
            return false;
        }
        let (body, checksum) = raw[1..].split_at(T::VERSIONED_LEN);
        keyed_checksum(key.as_ref(), body) == checksum
    }

    /// Verifies the checksum for this entry under `key`.
    ///
    /// Returns `true` if and only if:
    /// `self.checksum == blake2b256(encoded_key || item.serialize())`.
    ///
    /// # Key requirements
    /// `key` must be the exact byte encoding used as the LMDB key for this record.
    ///
    /// # Usage
    /// Callers should treat a checksum mismatch as a corruption or incompatibility signal and
    /// return a hard error (or trigger a rebuild path), depending on context. When the raw
    /// stored bytes are available, prefer [`Self::verify_stored`].
    pub(crate) fn verify<K: AsRef<[u8]>>(&self, key: K) -> bool {
        // Iterate from latest (T::VERSION) down to 1 (inclusive).
        let mut v = T::VERSION;
        loop {
            // Try to obtain the encoded bytes for this candidate version (tag + body).
            match self.item.to_bytes_with_version(v) {
                Ok(item_bytes) => {
                    // Compute the candidate checksum over (encoded_key || item_bytes).
                    let candidate = keyed_checksum(key.as_ref(), &item_bytes);
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
    ///
    /// This is the hashing primitive used by both wrappers. The checksum is not keyed.
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
    /// - StoredEntryFixed version tag
    /// - inner item bytes encoded with `item_version`
    /// - checksum over `encoded_key || inner_item_bytes`
    ///
    /// This method returns serialized bytes directly because `StoredEntryFixed<T>` does not store
    /// the inner item version. Constructing `Self` alone would lose the requested item version
    /// before the value is written.
    #[cfg(test)]
    pub(crate) fn to_bytes_with_item_version<K: AsRef<[u8]>>(
        key: K,
        item: &T,
        item_version: u8,
    ) -> io::Result<Vec<u8>> {
        let item_bytes = item.to_bytes_with_version(item_version)?;

        if item_bytes.len() != T::VERSIONED_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "encoded fixed-length item has an unexpected length",
            ));
        }

        let checksum = keyed_checksum(key.as_ref(), &item_bytes);

        let mut stored_entry_bytes = Vec::with_capacity(1 + T::VERSIONED_LEN + 32);
        stored_entry_bytes.push(Self::VERSION);
        stored_entry_bytes.extend_from_slice(&item_bytes);
        stored_entry_bytes.extend_from_slice(&checksum);

        Ok(stored_entry_bytes)
    }
}

/// Versioned on-disk encoding for fixed-length checksummed entries.
///
/// Body layout (after the `StoredEntryFixed` version tag):
/// 1. `T::serialize()` bytes (fixed length: `T::VERSIONED_LEN`)
/// 2. 32-byte checksum
///
/// Note: `T::serialize()` includes `T`’s own version tag and body.
impl<T: ZainoVersionedSerde + FixedEncodedLen> ZainoVersionedSerde for StoredEntryFixed<T> {
    const VERSION: u8 = version::V1;

    fn encode_latest<W: Write>(&self, w: &mut W) -> io::Result<()> {
        Self::encode_v1(self, w)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn encode_v1<W: Write>(&self, w: &mut W) -> io::Result<()> {
        self.item.serialize(&mut *w)?;
        write_fixed_le::<32, _>(&mut *w, &self.checksum)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut body = vec![0u8; T::VERSIONED_LEN];
        r.read_exact(&mut body)?;
        let item = T::deserialize(&body[..])?;

        let checksum = read_fixed_le::<32, _>(r)?;
        Ok(Self { item, checksum })
    }
}

/// `StoredEntryFixed<T>` has a fixed encoded body length.
///
/// Body length = `T::VERSIONED_LEN` + 32 bytes checksum.
impl<T: ZainoVersionedSerde + FixedEncodedLen> FixedEncodedLen for StoredEntryFixed<T> {
    const ENCODED_LEN: usize = T::VERSIONED_LEN + 32;
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
        let checksum = keyed_checksum(key.as_ref(), &body);
        Self { item, checksum }
    }

    /// Encodes the complete stored-entry wire bytes for `item` under `key` in a single
    /// pass: wrapper version tag, CompactSize length, item bytes, and the checksum over
    /// `key ‖ item bytes`.
    ///
    /// This is the write-path counterpart of `from_bytes`. Prefer it over
    /// `Self::new(key, item).to_bytes()`, which serializes the item twice (once for the
    /// checksum, once for the wire bytes).
    pub(crate) fn encode<K: AsRef<[u8]>>(key: K, item: &T) -> io::Result<Vec<u8>> {
        let item_bytes = item.to_bytes()?;
        let mut bytes = Vec::with_capacity(1 + 9 + item_bytes.len() + CHECKSUM_LEN);
        bytes.push(Self::VERSION);
        CompactSize::write(&mut bytes, item_bytes.len())?;
        bytes.extend_from_slice(&item_bytes);
        let checksum = keyed_checksum(key.as_ref(), &item_bytes);
        bytes.extend_from_slice(&checksum);
        Ok(bytes)
    }

    /// Verifies the checksum of raw stored-entry bytes under `key` without decoding or
    /// re-encoding the item: the checksum is defined over `key ‖ item bytes`, and both
    /// already sit in `raw` exactly as written.
    ///
    /// Version-agnostic by construction — the checksum binds whatever encoding version
    /// the writer used — and therefore both cheaper and stricter than [`Self::verify`],
    /// which re-encodes the decoded item per candidate version.
    pub(crate) fn verify_stored<K: AsRef<[u8]>>(key: K, raw: &[u8]) -> bool {
        let Some((_tag, mut rest)) = raw.split_first() else {
            return false;
        };
        let Ok(len) = CompactSize::read(&mut rest) else {
            return false;
        };
        let Ok(len) = usize::try_from(len) else {
            return false;
        };
        if rest.len() != len + CHECKSUM_LEN {
            return false;
        }
        let (body, checksum) = rest.split_at(len);
        keyed_checksum(key.as_ref(), body) == checksum
    }

    /// Verifies the checksum for this entry under `key`.
    ///
    /// Returns `true` if and only if:
    /// `self.checksum == blake2b256(encoded_key || item.serialize())`.
    ///
    /// # Key requirements
    /// `key` must be the exact byte encoding used as the LMDB key for this record.
    /// When the raw stored bytes are available, prefer [`Self::verify_stored`].
    pub(crate) fn verify<K: AsRef<[u8]>>(&self, key: K) -> bool {
        // Iterate from latest (T::VERSION) down to 1 (inclusive).
        let mut v = T::VERSION;
        loop {
            // Try to obtain the encoded bytes for this candidate version (tag + body).
            match self.item.to_bytes_with_version(v) {
                Ok(item_bytes) => {
                    // Compute the candidate checksum over (encoded_key || item_bytes).
                    let candidate = keyed_checksum(key.as_ref(), &item_bytes);
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
        let checksum = keyed_checksum(key.as_ref(), &item_bytes);

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

    // TestInner: versioned type with two encodings:
    // - v1: x as little-endian (body only)
    // - v2: x as big-endian (body only) and v2 is the current version
    impl ZainoVersionedSerde for TestInner {
        const VERSION: u8 = version::V2;

        fn encode_latest<W: Write>(&self, w: &mut W) -> io::Result<()> {
            self.encode_v2(w)
        }
        fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
            Self::decode_v2(r)
        }

        fn encode_v1<W: Write>(&self, w: &mut W) -> io::Result<()> {
            write_u32_le(w, self.x)
        }
        fn encode_v2<W: Write>(&self, w: &mut W) -> io::Result<()> {
            write_u32_be(w, self.x)
        }

        fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
            let x = read_u32_le(r)?;
            Ok(TestInner { x })
        }
        fn decode_v2<R: Read>(r: &mut R) -> io::Result<Self> {
            let x = read_u32_be(r)?;
            Ok(TestInner { x })
        }
    }

    // Make TestInner fixed-length for StoredEntryFixed tests.
    impl FixedEncodedLen for TestInner {
        // body length (no version tag): 4 bytes (u32)
        const ENCODED_LEN: usize = 4;
    }

    // Helper: simple key bytes for tests
    fn key_bytes() -> Vec<u8> {
        b"test-key".to_vec()
    }

    // StoredEntryFixed: latest roundtrip (new -> to_bytes -> from_bytes -> verify)
    #[test]
    fn stored_entry_fixed_roundtrip_latest() {
        let inner = TestInner { x: 0x1122_3344 };
        let key = key_bytes();

        // Construct wrapper using current serializer (StoredEntryFixed::new uses latest encoding)
        let wrapper = StoredEntryFixed::new(&key, inner.clone());

        // Verify should succeed for the same key
        assert!(
            wrapper.verify(&key),
            "wrapper verify (latest) should succeed"
        );

        // Encode wrapper to bytes and decode back
        let bytes = wrapper.to_bytes().expect("wrapper to_bytes");
        let parsed = StoredEntryFixed::<TestInner>::from_bytes(&bytes).expect("from_bytes");
        assert_eq!(parsed.item, inner);
        assert_eq!(parsed.checksum, wrapper.checksum);

        // parsed wrapper should verify with same key
        assert!(parsed.verify(&key));
    }

    #[test]
    // StoredEntryFixed: historical v1 body present on disk -> from_bytes + verify must succeed
    fn stored_entry_fixed_verify_old_v1() {
        let inner = TestInner { x: 0xAABB_CCDD };
        let key = key_bytes();

        // Produce item bytes according to historical v1 (tag + body)
        let item_bytes_v1 = inner
            .to_bytes_with_version(version::V1)
            .expect("inner v1 bytes");

        // Compute checksum over (key || item_bytes_v1)
        let mut digest_input = Vec::with_capacity(key.len() + item_bytes_v1.len());
        digest_input.extend_from_slice(&key);
        digest_input.extend_from_slice(&item_bytes_v1);
        let checksum = StoredEntryFixed::<TestInner>::blake2b256(&digest_input);

        // Manually build on-disk raw bytes for StoredEntryFixed:
        // [StoredEntryFixed::VERSION] + item_bytes_v1 (should have length T::VERSIONED_LEN) + checksum
        let mut raw = Vec::new();
        raw.push(StoredEntryFixed::<TestInner>::VERSION);
        raw.extend_from_slice(&item_bytes_v1);
        raw.extend_from_slice(&checksum);

        // Parse using from_bytes (which will call decode_v1 and reconstruct wrapper)
        let parsed = StoredEntryFixed::<TestInner>::from_bytes(&raw).expect("from_bytes v1");
        // parsed.item should equal the decoded inner
        assert_eq!(parsed.item, inner);
        // verify should succeed using the same key (it will try v2 then v1 candidate encodings)
        assert!(parsed.verify(&key));
    }

    // StoredEntryFixed: verify fails on tampered checksum or key
    #[test]
    fn stored_entry_fixed_verify_tamper() {
        let inner = TestInner { x: 0x0102_0304 };
        let key = key_bytes();

        let mut wrapper = StoredEntryFixed::new(&key, inner.clone());
        assert!(wrapper.verify(&key));

        // Tamper checksum (flip a byte) and ensure verify fails
        wrapper.checksum[0] ^= 0xff;
        assert!(
            !wrapper.verify(&key),
            "verify should fail with tampered checksum"
        );

        // Restore checksum and verify ok, then check wrong key fails
        wrapper = StoredEntryFixed::new(&key, inner.clone());
        assert!(wrapper.verify(&key));
        let wrong_key = b"other-key".to_vec();
        assert!(
            !wrapper.verify(&wrong_key),
            "verify should fail with wrong key"
        );
    }

    // -------------------- StoredEntryVar tests --------------------

    #[test]
    fn stored_entry_var_roundtrip_latest() {
        let inner = TestInner { x: 0x5566_7788 };
        let key = key_bytes();

        let wrapper = StoredEntryVar::new(&key, inner.clone());
        assert!(
            wrapper.verify(&key),
            "var wrapper verify (latest) should succeed"
        );

        // Encode wrapper to bytes and decode back via From/To bytes
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

        // item serialized as v1 (tag + body)
        let item_bytes_v1 = inner
            .to_bytes_with_version(version::V1)
            .expect("inner v1 bytes");

        // checksum computed over (key || item_bytes_v1)
        let mut digest_input = Vec::with_capacity(key.len() + item_bytes_v1.len());
        digest_input.extend_from_slice(&key);
        digest_input.extend_from_slice(&item_bytes_v1);
        let checksum = StoredEntryVar::<TestInner>::blake2b256(&digest_input);

        // Build raw stored value for StoredEntryVar:
        // [StoredEntryVar::VERSION] + CompactSize(len) + item_bytes_v1 + checksum
        let mut raw = Vec::new();
        raw.push(StoredEntryVar::<TestInner>::VERSION);
        CompactSize::write(&mut raw, item_bytes_v1.len()).expect("write compactsize");
        raw.extend_from_slice(&item_bytes_v1);
        // write checksum as fixed 32 bytes
        write_fixed_le::<32, _>(&mut raw, &checksum).expect("write checksum");

        // from_bytes should parse the body and return wrapper
        let parsed = StoredEntryVar::<TestInner>::from_bytes(&raw).expect("var from_bytes v1");
        assert_eq!(parsed.item, inner);
        // verify must succeed using same key (it will try v2 then v1)
        assert!(parsed.verify(&key));
    }

    #[test]
    fn stored_entry_var_verify_tamper() {
        let inner = TestInner { x: 0xCAFEBABE };
        let key = key_bytes();

        let mut wrapper = StoredEntryVar::new(&key, inner);
        assert!(wrapper.verify(&key));

        // tamper checksum
        wrapper.checksum[31] ^= 0xff;
        assert!(!wrapper.verify(&key));

        // restore and test wrong key fails
        let wrapper = StoredEntryVar::new(&key, TestInner { x: 0xCAFEBABE });
        let wrong_key = b"bad-key".to_vec();
        assert!(!wrapper.verify(&wrong_key));
    }

    // -------------------- single-pass encode / stored-bytes verify --------------------

    #[test]
    fn encode_matches_new_then_to_bytes() {
        let key = key_bytes();

        let inner = TestInner { x: 0x0123_4567 };
        let fixed_single_pass = StoredEntryFixed::encode(&key, &inner).expect("fixed encode");
        let fixed_double_pass = StoredEntryFixed::new(&key, inner.clone())
            .to_bytes()
            .expect("fixed to_bytes");
        assert_eq!(fixed_single_pass, fixed_double_pass);

        let var_single_pass = StoredEntryVar::encode(&key, &inner).expect("var encode");
        let var_double_pass = StoredEntryVar::new(&key, inner)
            .to_bytes()
            .expect("var to_bytes");
        assert_eq!(var_single_pass, var_double_pass);
    }

    #[test]
    fn verify_stored_accepts_written_bytes_and_rejects_tampering() {
        let key = key_bytes();
        let inner = TestInner { x: 0x89AB_CDEF };

        let fixed = StoredEntryFixed::encode(&key, &inner).expect("fixed encode");
        assert!(StoredEntryFixed::<TestInner>::verify_stored(&key, &fixed));
        assert!(!StoredEntryFixed::<TestInner>::verify_stored(
            b"other-key",
            &fixed
        ));
        let mut tampered = fixed.clone();
        tampered[1] ^= 0xff;
        assert!(!StoredEntryFixed::<TestInner>::verify_stored(
            &key, &tampered
        ));
        assert!(!StoredEntryFixed::<TestInner>::verify_stored(
            &key,
            &fixed[..fixed.len() - 1]
        ));

        let var = StoredEntryVar::encode(&key, &inner).expect("var encode");
        assert!(StoredEntryVar::<TestInner>::verify_stored(&key, &var));
        assert!(!StoredEntryVar::<TestInner>::verify_stored(
            b"other-key",
            &var
        ));
        let mut tampered = var.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0xff;
        assert!(!StoredEntryVar::<TestInner>::verify_stored(&key, &tampered));
    }

    #[test]
    fn verify_stored_accepts_historical_item_versions() {
        // verify_stored binds whatever bytes were written, so values encoded with an
        // older inner item version still verify without any re-encoding loop.
        let key = key_bytes();
        let inner = TestInner { x: 0xAABB_CCDD };

        let fixed_raw =
            StoredEntryFixed::<TestInner>::to_bytes_with_item_version(&key, &inner, version::V1)
                .expect("fixed v1 bytes");
        assert!(StoredEntryFixed::<TestInner>::verify_stored(
            &key, &fixed_raw
        ));

        let var_raw =
            StoredEntryVar::<TestInner>::to_bytes_with_item_version(&key, &inner, version::V1)
                .expect("var v1 bytes");
        assert!(StoredEntryVar::<TestInner>::verify_stored(&key, &var_raw));
    }
}
