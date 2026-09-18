//! The domain persistence port: typed, versioned entries over the low KV port.
//!
//! Two persistence seams. The **low KV port** ([`zaino_persistence::Backend`])
//! stores raw bytes and knows nothing of domain types or versions — LMDB and the
//! in-memory backend implement it, and it stays that dumb on purpose. This crate
//! is the **domain persistence port** over it: an [`EntryCodec`] maps an index's
//! typed `Key`/`Value` to on-disk bytes at a declared [`FormatVersion`], and on
//! open a recorded-version mismatch *rejects* the persisted bytes so the caller
//! **rebuilds** the index from source rather than migrating it.
//!
//! A codec owns **format**, not **placement**: the namespace an index lives in is
//! the caller's concern (it already names its indexes), so every helper takes the
//! namespace explicitly rather than the codec carrying it. That keeps a single
//! source of truth for the namespace.
//!
//! That is the whole point of the version tag: it is a *guard*, not a migration
//! engine. Because a mismatch discards and rebuilds, only the current version's
//! codec ever exists — no per-version type zoo, no transforms. A rebuild is a
//! reach event (the index climbs from empty again), never a presence one: the
//! code still has the index; only its persisted data was thrown away.
#![forbid(unsafe_code)]

use zaino_persistence::{BackendReader, Namespace, ReadError, WriteOp};

/// Metadata namespace recording each index namespace's on-disk format version.
/// Separate from the index namespaces so a version stamp never collides with a
/// real key.
const VERSION_META: Namespace = Namespace::new("_format_versions");

/// A per-namespace on-disk format version.
///
/// Bumping it declares previously persisted bytes for that namespace unreadable:
/// on open a mismatch is rejected and the index rebuilt from source, never
/// migrated. Keeping it a `u16` is deliberate — it is a monotonic tag, not a
/// semver.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FormatVersion(pub u16);

/// Failure to decode persisted bytes back into a domain value — the disk→domain
/// validation step.
///
/// Distinct from a version mismatch: a mismatch is an expected upgrade, whereas
/// a decode failure within the *claimed-correct* version is corruption or a bug.
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    /// The byte slice has the wrong length or format.
    #[error("{0}")]
    Invalid(String),
}

/// The codec for one index's entries: its typed `Key`/`Value` ↔ on-disk bytes,
/// at a fixed [`FormatVersion`].
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

    /// The on-disk format version. Bump to force reject-and-rebuild.
    const VERSION: FormatVersion;

    /// Encode a key to its on-disk bytes.
    fn encode_key(key: &Self::Key) -> Vec<u8>;
    /// Encode a value to its on-disk bytes.
    fn encode_value(value: &Self::Value) -> Vec<u8>;
    /// Decode a key from its on-disk bytes — a validation boundary.
    fn decode_key(bytes: &[u8]) -> Result<Self::Key, DecodeError>;
    /// Decode a value from its on-disk bytes — a validation boundary.
    fn decode_value(bytes: &[u8]) -> Result<Self::Value, DecodeError>;
}

/// Whether a namespace's persisted data is usable by the running code.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Freshness {
    /// The recorded version matches the codec — the data may be read.
    Fresh,
    /// No version recorded, or it does not match — the data is unusable and the
    /// index must be rebuilt from source.
    Stale,
}

/// The [`WriteOp`] that stamps `namespace` with codec `C`'s current version.
///
/// A writer includes this when it (re)builds the namespace, so a later open can
/// tell whether the persisted bytes match the running code.
pub fn version_stamp<C: EntryCodec>(namespace: Namespace) -> WriteOp {
    WriteOp::Put {
        namespace: VERSION_META,
        key: namespace.as_str().as_bytes().to_vec(),
        value: C::VERSION.0.to_le_bytes().to_vec(),
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

/// The version recorded on disk for `namespace`, if any. A malformed stamp reads
/// as absent — treated as [`Freshness::Stale`], the safe direction.
pub fn recorded_version(
    reader: &dyn BackendReader,
    namespace: Namespace,
) -> Result<Option<FormatVersion>, ReadError> {
    let raw = reader.get(VERSION_META, namespace.as_str().as_bytes())?;
    Ok(raw.and_then(|bytes| {
        let tag: [u8; 2] = bytes.as_slice().try_into().ok()?;
        Some(FormatVersion(u16::from_le_bytes(tag)))
    }))
}

/// Whether codec `C`'s persisted data in `namespace` matches the running code.
pub fn freshness<C: EntryCodec>(
    reader: &dyn BackendReader,
    namespace: Namespace,
) -> Result<Freshness, ReadError> {
    Ok(match recorded_version(reader, namespace)? {
        Some(version) if version == C::VERSION => Freshness::Fresh,
        _ => Freshness::Stale,
    })
}

/// Failure loading typed entries for a namespace.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    /// The low KV backend read failed.
    #[error("backend read: {0}")]
    Backend(#[from] ReadError),
    /// Persisted bytes did not decode — corruption within the claimed version,
    /// not an expected upgrade.
    #[error("decode: {0}")]
    Decode(#[from] DecodeError),
}

/// The decoded entries of one namespace, as produced by [`load`].
pub type Entries<C> = Vec<(<C as EntryCodec>::Key, <C as EntryCodec>::Value)>;

/// Load and decode every entry in `namespace` with codec `C`.
///
/// Call only after [`freshness`] returns [`Freshness::Fresh`]; on `Stale` the
/// caller rebuilds instead of reading. A [`LoadError::Decode`] here means
/// corruption within the claimed-correct version.
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

    /// A toy codec at version 1.
    struct Toy;
    impl EntryCodec for Toy {
        type Key = u32;
        type Value = u64;
        const VERSION: FormatVersion = FormatVersion(1);

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
    }

    /// The same entries with a bumped version — a code upgrade.
    struct ToyV2;
    impl EntryCodec for ToyV2 {
        type Key = u32;
        type Value = u64;
        const VERSION: FormatVersion = FormatVersion(2);

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
    }

    fn commit(backend: &InMemoryBackend, ops: Vec<WriteOp>) {
        let mut writer = backend.writer().expect("writer");
        writer.commit(ops).expect("commit");
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
        commit(&backend, vec![put::<Toy>(TOY, &1, &1)]); // data, no version stamp
        let reader = backend.reader().expect("reader");
        assert_eq!(
            freshness::<Toy>(&reader, TOY).expect("freshness"),
            Freshness::Stale
        );
    }

    #[test]
    fn a_bumped_version_rejects_old_data_for_rebuild() {
        let backend = InMemoryBackend::new();
        // Written by v1 code.
        commit(
            &backend,
            vec![version_stamp::<Toy>(TOY), put::<Toy>(TOY, &1, &1)],
        );
        let reader = backend.reader().expect("reader");

        // v2 code opens it: rejected → the caller rebuilds, never migrates.
        assert_eq!(
            freshness::<ToyV2>(&reader, TOY).expect("freshness"),
            Freshness::Stale
        );
        // v1 code still reads it.
        assert_eq!(
            freshness::<Toy>(&reader, TOY).expect("freshness"),
            Freshness::Fresh
        );
    }
}
