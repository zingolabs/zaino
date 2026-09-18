//! The finalised-tip watermark — the writer→reader coordination value.
//!
//! The indexer commits the highest finalised height it has built; a reader
//! consumes it to bound its serviceable range. Both talk to this one place, so
//! neither hand-rolls the namespace, key, or encoding — the sync engine says
//! [`stamp`], a reader says [`read`], and the storage details stay here at the
//! persistence seam.
//!
//! The value is a raw block height (`u64`): this layer stores a number, and each
//! side interprets it in its own height type. That keeps the persistence port
//! free of any concrete domain-height dependency.

use zaino_persistence::{BackendReader, Namespace, ReadError, WriteOp};

/// Namespace for the watermark — separate from any index's namespace.
const NAMESPACE: Namespace = Namespace::new("_watermark");
/// The single key under [`NAMESPACE`].
const KEY: &[u8] = b"finalised_tip";

/// The [`WriteOp`] that records `height` as the finalised-tip watermark.
///
/// The writer includes it in the same atomic batch as the entries it commits, so
/// the watermark can never lead the data it vouches for.
pub fn stamp(height: u64) -> WriteOp {
    WriteOp::Put {
        namespace: NAMESPACE,
        key: KEY.to_vec(),
        value: height.to_le_bytes().to_vec(),
    }
}

/// The recorded finalised-tip watermark, if any.
///
/// A malformed value reads as absent — the safe direction: a reader treats it as
/// "nothing finalised", and the writer resumes from genesis.
pub fn read(reader: &dyn BackendReader) -> Result<Option<u64>, ReadError> {
    Ok(reader.get(NAMESPACE, KEY)?.and_then(|bytes| {
        let value: [u8; 8] = bytes.as_slice().try_into().ok()?;
        Some(u64::from_le_bytes(value))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaino_persistence::in_memory::InMemoryBackend;
    use zaino_persistence::{Backend, BackendWriter};

    #[test]
    fn round_trips_the_watermark() {
        let backend = InMemoryBackend::new();
        let mut writer = backend.writer().expect("writer");
        writer.commit(vec![stamp(42)]).expect("commit");
        let reader = backend.reader().expect("reader");
        assert_eq!(read(&reader).expect("read"), Some(42));
    }

    #[test]
    fn absent_watermark_reads_as_none() {
        let backend = InMemoryBackend::new();
        let reader = backend.reader().expect("reader");
        assert_eq!(read(&reader).expect("read"), None);
    }
}
