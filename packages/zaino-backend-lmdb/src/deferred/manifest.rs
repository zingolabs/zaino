//! The deferral manifest: the durable record of which namespaces are deferred
//! and how far their run logs are committed.
//!
//! The manifest lives in a reserved [`Meta`](zaino_persistence::KeyOrder::Meta)
//! namespace, [`MANIFEST_NAMESPACE`], that [`LmdbBackend::open`](crate::LmdbBackend::open)
//! registers automatically — callers never declare it. Within it:
//!
//! - key `"<ns>"` → [`run entry`](RunEntry) `(segment_count, log_len)`: the
//!   namespace `<ns>` is deferred, its run log holds `segment_count` segments in
//!   `log_len` committed bytes. Presence of this key *is* incompleteness
//!   ([`is_complete`](crate::LmdbReader::is_complete) returns `false`).
//! - key `"<ns>/loaded_through"` → the last key [`finish_bulk`](crate::LmdbBackend::finish_bulk)
//!   appended into the tree, so a crash mid-merge resumes past it.
//!
//! The run entry is written in the *same* LMDB transaction as the watermark, so
//! the committed `log_len` and the watermark advance atomically. Every value is a
//! plain overwriting put (meta keys are never appended).

use zaino_persistence::{Namespace, RawKey};

/// The reserved meta namespace holding the deferral manifest.
///
/// The leading underscore matches the other reserved namespaces (`_watermark`),
/// and no index codec ever produces a namespace name, so a collision with a
/// caller-declared namespace cannot arise.
pub(crate) const MANIFEST_NAMESPACE: Namespace = Namespace::new("_deferred_manifest");

/// The suffix distinguishing a `loaded_through` key from a run entry under the
/// same namespace prefix.
const LOADED_THROUGH_SUFFIX: &[u8] = b"/loaded_through";

/// A decoded run-manifest entry: how far a namespace's run log is committed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RunEntry {
    /// Number of segments appended (one per bulk commit that touched the namespace).
    pub(crate) segment_count: u64,
    /// Committed byte length of the run log. Bytes past it belong to a batch whose
    /// watermark never committed and are truncated on reopen.
    pub(crate) log_len: u64,
}

impl RunEntry {
    /// Encode to the 16-byte big-endian value stored under the `"<ns>"` key.
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16);
        out.extend_from_slice(&self.segment_count.to_be_bytes());
        out.extend_from_slice(&self.log_len.to_be_bytes());
        out
    }

    /// Decode from the stored value, or `None` if it is not the expected 16 bytes.
    pub(crate) fn decode(bytes: &[u8]) -> Option<Self> {
        let raw: [u8; 16] = bytes.try_into().ok()?;
        let segment_count = u64::from_be_bytes(raw[0..8].try_into().expect("8-byte segment count"));
        let log_len = u64::from_be_bytes(raw[8..16].try_into().expect("8-byte log len"));
        Some(Self {
            segment_count,
            log_len,
        })
    }
}

/// The manifest key for a namespace's run entry: the namespace name itself.
pub(crate) fn run_key(namespace: Namespace) -> RawKey {
    namespace.as_str().as_bytes().to_vec()
}

/// The manifest key for a namespace's `loaded_through` resume point.
pub(crate) fn loaded_through_key(namespace: Namespace) -> RawKey {
    let mut key = namespace.as_str().as_bytes().to_vec();
    key.extend_from_slice(LOADED_THROUGH_SUFFIX);
    key
}

/// Whether a raw manifest key is a run entry (rather than a `loaded_through` key).
///
/// Used when scanning the manifest to enumerate the deferred namespaces: a run
/// entry's key is a bare namespace name, so it is any key that does not carry the
/// [`LOADED_THROUGH_SUFFIX`].
pub(crate) fn is_run_key(key: &[u8]) -> bool {
    !key.ends_with(LOADED_THROUGH_SUFFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_entry_round_trips() {
        let entry = RunEntry {
            segment_count: 3,
            log_len: 4096,
        };
        assert_eq!(RunEntry::decode(&entry.encode()), Some(entry));
    }

    #[test]
    fn run_entry_rejects_wrong_length() {
        assert_eq!(RunEntry::decode(&[0u8; 8]), None);
        assert_eq!(RunEntry::decode(&[0u8; 24]), None);
    }

    #[test]
    fn keys_are_distinct_and_classifiable() {
        let ns = Namespace::new("address_history");
        let run = run_key(ns);
        let loaded = loaded_through_key(ns);
        assert_ne!(run, loaded);
        assert!(is_run_key(&run), "a bare namespace name is a run entry");
        assert!(
            !is_run_key(&loaded),
            "a loaded_through key is not a run entry"
        );
        assert!(
            loaded.starts_with(&run),
            "loaded_through extends the run key, so a prefix scan would see both"
        );
    }
}
