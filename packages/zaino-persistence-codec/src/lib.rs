//! The domain persistence port: typed, versioned entries over the low KV port.
//!
//! Two persistence seams. The **low KV port** ([`zaino_persistence::Backend`])
//! stores raw bytes and knows nothing of domain types or versions — LMDB and the
//! in-memory backend implement it, and it stays that dumb on purpose. This crate
//! is the **domain persistence port** over it: an [`EntryCodec`] maps an index's
//! typed `Key`/`Value` to on-disk bytes, and on open a version mismatch *rejects*
//! the persisted bytes so the caller **rebuilds** the index from source rather
//! than migrating it.
//!
//! # The version is a fingerprint of the format, not a hand-set number
//!
//! A hand-maintained version number can silently desync from the format: change
//! `encode` without bumping the number and old bytes are read under the new
//! layout. So the version is **derived** — [`format_version`] hashes a codec's
//! canonical samples, encoded. Any change to the byte layout of a covered value
//! changes the fingerprint and triggers reject-and-rebuild; there is no number
//! to forget to bump. This works precisely because we never migrate: any format
//! change means rebuild, which is exactly what a fingerprint gives.
//!
//! Coverage of the fingerprint is the codec author's job: [`EntryCodec::fingerprint_samples`]
//! must exercise every field and variant of the on-disk format. Constructing the
//! samples with all fields explicit (no `..Default`) makes the compiler force a
//! new field into the sample, so the fingerprint moves when the format grows.
//!
//! ## Caveat: sample coverage is a convention, not yet enforced (deferred)
//!
//! The desync guarantee holds only as far as the samples cover the format, and
//! that coverage is currently **convention**, not a lint. Two soft spots:
//!
//! - **Fields.** The "every field explicit, no `..Default`" rule above is what
//!   makes the compiler force a new field into a sample. Nothing *enforces* it —
//!   a `..Default::default()` in a future sample would silently reopen the
//!   desync gap for any field added afterwards.
//! - **Variants.** Adding an enum variant does not break an exhaustive struct
//!   literal, so a new format-affecting variant is covered only if the author
//!   remembers to add a sample for it (see `address_history`'s `ScriptType`,
//!   hand-listed today).
//!
//! Hardening is **deferred**. When it earns its keep: (1) a grep-based CI lint
//! (a `makers lint-*` task, like the boundary-conversion lint) that fails on
//! `..` inside a `fingerprint_samples` body — makes the field rule hard; (2)
//! give each format-affecting enum an exhaustive-checked `ALL` and iterate it in
//! `fingerprint_samples`, so variant coverage becomes structural; (3) a
//! `#[derive]` that generates exhaustive samples, removing the discipline
//! entirely. Until then, **treat sample coverage as a review checklist** when
//! adding or changing a codec.
//!
//! A codec owns **format**, not **placement**: the namespace is the caller's
//! concern, supplied to every helper.
#![forbid(unsafe_code)]

pub mod watermark;

use zaino_persistence::{BackendReader, Namespace, ReadError, WriteOp};

/// Metadata namespace recording each index namespace's on-disk format version.
/// Separate from the index namespaces so a version stamp never collides with a
/// real key.
const VERSION_META: Namespace = Namespace::new("_format_versions");

/// A namespace's on-disk format fingerprint — a hash of its codec's canonical
/// samples, encoded (see [`format_version`]).
///
/// Not a semver: it is an opaque tag whose only meaning is equality. A different
/// fingerprint means the persisted bytes were written by a different format and
/// must be discarded, never migrated.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FormatVersion(pub u64);

/// Failure to decode persisted bytes back into a domain value — the disk→domain
/// validation step.
///
/// Distinct from a version mismatch: a mismatch is an expected upgrade, whereas
/// a decode failure within the *fingerprint-matched* format is corruption or a
/// bug.
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    /// The byte slice has the wrong length or format.
    #[error("{0}")]
    Invalid(String),
}

/// The codec for one index's entries: its typed `Key`/`Value` ↔ on-disk bytes.
///
/// This is the DTO boundary — `decode_*` *is* the disk→domain validation step.
/// It replaces the byte codec that used to live on the sync engine's `Schema`
/// trait, so `Schema` can shrink to a pure domain projection. It owns format
/// only; the namespace is supplied by the caller.
pub trait EntryCodec {
    /// The typed key.
    type Key;
    /// The typed value.
    type Value;

    /// Encode a key to its on-disk bytes.
    fn encode_key(key: &Self::Key) -> Vec<u8>;
    /// Encode a value to its on-disk bytes.
    fn encode_value(value: &Self::Value) -> Vec<u8>;
    /// Decode a key from its on-disk bytes — a validation boundary.
    fn decode_key(bytes: &[u8]) -> Result<Self::Key, DecodeError>;
    /// Decode a value from its on-disk bytes — a validation boundary.
    fn decode_value(bytes: &[u8]) -> Result<Self::Value, DecodeError>;

    /// Canonical sample entries that characterise this codec's on-disk format.
    ///
    /// The [`format_version`] fingerprint is the hash of these, encoded — so the
    /// on-disk version tracks the format automatically. Construct each sample
    /// with **every field explicit** (no `..Default`) and cover every enum
    /// variant, so a format change cannot escape the fingerprint. The values
    /// need not be meaningful; they only need to exercise the layout.
    fn fingerprint_samples() -> Vec<(Self::Key, Self::Value)>;
}

/// A deterministic 64-bit FNV-1a hash.
///
/// Deterministic across runs and platforms — which a persisted fingerprint must
/// be, unlike Rust's randomized default hasher. Not cryptographic: it only has
/// to change when the format changes, and a local guard has no adversary.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The format fingerprint for codec `C`: a hash of its canonical samples,
/// encoded and length-framed so key/value boundaries cannot alias.
///
/// This is the on-disk version. It changes iff the encoded bytes of a covered
/// value change, so the version can never silently desync from the format.
pub fn format_version<C: EntryCodec>() -> FormatVersion {
    let mut framed = Vec::new();
    for (key, value) in C::fingerprint_samples() {
        for blob in [C::encode_key(&key), C::encode_value(&value)] {
            let len = u64::try_from(blob.len()).expect("canonical sample length fits u64");
            framed.extend_from_slice(&len.to_le_bytes());
            framed.extend_from_slice(&blob);
        }
    }
    FormatVersion(fnv1a(&framed))
}

/// Whether a namespace's persisted data is usable by the running code.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Freshness {
    /// The recorded fingerprint matches the codec — the data may be read.
    Fresh,
    /// No fingerprint recorded, or it does not match — the data is unusable and
    /// the index must be rebuilt from source.
    Stale,
}

/// The [`WriteOp`] that stamps `namespace` with codec `C`'s current format
/// fingerprint.
///
/// A writer includes this when it (re)builds the namespace, so a later open can
/// tell whether the persisted bytes match the running code's format.
pub fn version_stamp<C: EntryCodec>(namespace: Namespace) -> WriteOp {
    WriteOp::Put {
        namespace: VERSION_META,
        key: namespace.as_str().as_bytes().to_vec(),
        value: format_version::<C>().0.to_le_bytes().to_vec(),
    }
}

/// A typed put for one entry into `namespace`.
pub fn put<C: EntryCodec>(namespace: Namespace, key: &C::Key, value: &C::Value) -> WriteOp {
    WriteOp::Put {
        namespace,
        key: C::encode_key(key),
        value: C::encode_value(value),
    }
}

/// The fingerprint recorded on disk for `namespace`, if any. A malformed stamp
/// reads as absent — treated as [`Freshness::Stale`], the safe direction.
pub fn recorded_version(
    reader: &dyn BackendReader,
    namespace: Namespace,
) -> Result<Option<FormatVersion>, ReadError> {
    let raw = reader.get(VERSION_META, namespace.as_str().as_bytes())?;
    Ok(raw.and_then(|bytes| {
        let tag: [u8; 8] = bytes.as_slice().try_into().ok()?;
        Some(FormatVersion(u64::from_le_bytes(tag)))
    }))
}

/// Whether codec `C`'s persisted data in `namespace` matches the running code's
/// format.
pub fn freshness<C: EntryCodec>(
    reader: &dyn BackendReader,
    namespace: Namespace,
) -> Result<Freshness, ReadError> {
    Ok(match recorded_version(reader, namespace)? {
        Some(recorded) if recorded == format_version::<C>() => Freshness::Fresh,
        _ => Freshness::Stale,
    })
}

/// Failure loading typed entries for a namespace.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    /// The low KV backend read failed.
    #[error("backend read: {0}")]
    Backend(#[from] ReadError),
    /// Persisted bytes did not decode — corruption within the fingerprint-matched
    /// format, not an expected upgrade.
    #[error("decode: {0}")]
    Decode(#[from] DecodeError),
}

/// The decoded entries of one namespace, as produced by [`load`].
pub type Entries<C> = Vec<(<C as EntryCodec>::Key, <C as EntryCodec>::Value)>;

/// Load and decode every entry in `namespace` with codec `C`.
///
/// Call only after [`freshness`] returns [`Freshness::Fresh`]; on `Stale` the
/// caller rebuilds instead of reading. A [`LoadError::Decode`] here means
/// corruption within the fingerprint-matched format.
pub fn load<C: EntryCodec>(
    reader: &dyn BackendReader,
    namespace: Namespace,
) -> Result<Entries<C>, LoadError> {
    reader
        .scan(namespace)?
        .into_iter()
        .map(
            |(raw_key, raw_value)| -> Result<(C::Key, C::Value), LoadError> {
                Ok((C::decode_key(&raw_key)?, C::decode_value(&raw_value)?))
            },
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaino_persistence::in_memory::InMemoryBackend;
    use zaino_persistence::{Backend, BackendWriter};

    const TOY: Namespace = Namespace::new("toy");

    /// A toy codec — little-endian.
    struct Toy;
    impl EntryCodec for Toy {
        type Key = u32;
        type Value = u64;

        fn encode_key(key: &u32) -> Vec<u8> {
            key.to_le_bytes().to_vec()
        }
        fn encode_value(value: &u64) -> Vec<u8> {
            value.to_le_bytes().to_vec()
        }
        fn decode_key(bytes: &[u8]) -> Result<u32, DecodeError> {
            let tag: [u8; 4] = bytes
                .try_into()
                .map_err(|_| DecodeError::Invalid("bad key width".to_owned()))?;
            Ok(u32::from_le_bytes(tag))
        }
        fn decode_value(bytes: &[u8]) -> Result<u64, DecodeError> {
            let tag: [u8; 8] = bytes
                .try_into()
                .map_err(|_| DecodeError::Invalid("bad value width".to_owned()))?;
            Ok(u64::from_le_bytes(tag))
        }
        fn fingerprint_samples() -> Vec<(u32, u64)> {
            vec![(1, 1), (u32::MAX, u64::MAX)]
        }
    }

    /// The same entries but a **different byte layout** (big-endian) — a format
    /// change that a hand-set version could forget to bump, but the fingerprint
    /// cannot.
    struct ToyBigEndian;
    impl EntryCodec for ToyBigEndian {
        type Key = u32;
        type Value = u64;

        fn encode_key(key: &u32) -> Vec<u8> {
            key.to_be_bytes().to_vec()
        }
        fn encode_value(value: &u64) -> Vec<u8> {
            value.to_be_bytes().to_vec()
        }
        fn decode_key(bytes: &[u8]) -> Result<u32, DecodeError> {
            let tag: [u8; 4] = bytes
                .try_into()
                .map_err(|_| DecodeError::Invalid("bad key width".to_owned()))?;
            Ok(u32::from_be_bytes(tag))
        }
        fn decode_value(bytes: &[u8]) -> Result<u64, DecodeError> {
            let tag: [u8; 8] = bytes
                .try_into()
                .map_err(|_| DecodeError::Invalid("bad value width".to_owned()))?;
            Ok(u64::from_be_bytes(tag))
        }
        fn fingerprint_samples() -> Vec<(u32, u64)> {
            vec![(1, 1), (u32::MAX, u64::MAX)]
        }
    }

    fn commit(backend: &InMemoryBackend, ops: Vec<WriteOp>) {
        let mut writer = backend.writer().expect("writer");
        writer.commit(ops).expect("commit");
    }

    #[test]
    fn a_codecs_fingerprint_is_stable() {
        assert_eq!(format_version::<Toy>(), format_version::<Toy>());
    }

    #[test]
    fn changing_the_byte_layout_changes_the_fingerprint() {
        // Same samples, same declared version-intent — only the encoding differs.
        // A hand-set number could stay equal here; the fingerprint must not.
        assert_ne!(format_version::<Toy>(), format_version::<ToyBigEndian>());
    }

    #[test]
    fn round_trips_typed_entries() {
        let backend = InMemoryBackend::new();
        commit(
            &backend,
            vec![
                version_stamp::<Toy>(TOY),
                put::<Toy>(TOY, &7, &42),
                put::<Toy>(TOY, &8, &99),
            ],
        );
        let reader = backend.reader().expect("reader");

        assert_eq!(
            freshness::<Toy>(&reader, TOY).expect("freshness"),
            Freshness::Fresh
        );
        let mut got = load::<Toy>(&reader, TOY).expect("load");
        got.sort_unstable();
        assert_eq!(got, vec![(7, 42), (8, 99)]);
    }

    #[test]
    fn an_unstamped_namespace_is_stale() {
        let backend = InMemoryBackend::new();
        commit(&backend, vec![put::<Toy>(TOY, &1, &1)]); // data, no stamp
        let reader = backend.reader().expect("reader");
        assert_eq!(
            freshness::<Toy>(&reader, TOY).expect("freshness"),
            Freshness::Stale
        );
    }

    #[test]
    fn a_changed_format_rejects_old_data_for_rebuild() {
        let backend = InMemoryBackend::new();
        // Written by the little-endian codec.
        commit(
            &backend,
            vec![version_stamp::<Toy>(TOY), put::<Toy>(TOY, &1, &1)],
        );
        let reader = backend.reader().expect("reader");

        // The big-endian codec opens it: fingerprint differs → rejected → rebuild.
        assert_eq!(
            freshness::<ToyBigEndian>(&reader, TOY).expect("freshness"),
            Freshness::Stale
        );
        // The original codec still reads it.
        assert_eq!(
            freshness::<Toy>(&reader, TOY).expect("freshness"),
            Freshness::Fresh
        );
    }
}
