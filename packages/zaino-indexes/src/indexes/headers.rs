//! HeadersIndex (BlockLocal × Append): height → (hash, prev_hash, time, bits).

use zaino_persistence_codec::keys::HeightKey;
use zaino_persistence_codec::{DecodeError, EntryCodec, PersistentRecord};
use zaino_primitives::types::{BlockHash, BlockTime, CompactDifficulty};
use zaino_sync::descriptor::{Append, BlockLocal};
use zaino_sync::primitives::{BlockHeight, IndexId};
use zaino_sync::traits::{ExtractLocal, IndexDef, MergeAppend, Schema};

/// Per-index context for HeadersIndex.
pub struct HeaderCtx {
    /// Block height.
    pub height: BlockHeight,
    /// Block hash.
    pub hash: BlockHash,
    /// Previous block hash.
    pub prev_hash: BlockHash,
    /// Timestamp.
    pub time: BlockTime,
    /// Compact difficulty.
    pub bits: CompactDifficulty,
}

/// Header delta.
pub struct HeaderEntry {
    /// Block height (key).
    pub height: BlockHeight,
    /// Header data (value).
    pub value: HeaderValue,
}

/// Persisted header value: hash(32) + prev_hash(32) + time(4) + bits(4) = 72 bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderValue {
    /// Block hash.
    pub hash: BlockHash,
    /// Previous block hash.
    pub prev_hash: BlockHash,
    /// Timestamp.
    pub time: BlockTime,
    /// Compact difficulty.
    pub bits: CompactDifficulty,
}

/// Headers index definition.
pub struct HeadersIndex;

/// Index identity.
pub const ID: IndexId = IndexId::new("headers");

impl IndexDef for HeadersIndex {
    type Scope = BlockLocal;
    type Composition = Append;
    type Delta = HeaderEntry;
    type BlockContext = HeaderCtx;

    const NAME: IndexId = ID;
}

impl ExtractLocal for HeadersIndex {
    type Error = std::convert::Infallible;

    fn extract(ctx: &HeaderCtx) -> Result<Self::Delta, Self::Error> {
        Ok(HeaderEntry {
            height: ctx.height,
            value: HeaderValue {
                hash: ctx.hash,
                prev_hash: ctx.prev_hash,
                time: ctx.time,
                bits: ctx.bits,
            },
        })
    }
}

impl MergeAppend for HeadersIndex {}

impl Schema<Vec<HeaderEntry>> for HeadersIndex {
    fn into_entries(entries: Vec<HeaderEntry>) -> Vec<(Self::Key, Self::Value)> {
        entries.into_iter().map(|e| (e.height, e.value)).collect()
    }

    fn from_entries(entries: Vec<(Self::Key, Self::Value)>) -> Vec<HeaderEntry> {
        entries
            .into_iter()
            .map(|(height, value)| HeaderEntry { height, value })
            .collect()
    }
}

impl EntryCodec for HeadersIndex {
    type Key = BlockHeight;
    type Value = HeaderValue;
    type PersistentKey = HeightKey<BlockHeight>;
    type PersistentValue = PersistentHeaderValue;

    fn fingerprint_samples() -> Vec<(BlockHeight, HeaderValue)> {
        vec![(
            BlockHeight::new(1),
            HeaderValue {
                hash: BlockHash::from([1u8; 32]),
                prev_hash: BlockHash::from([2u8; 32]),
                time: 3,
                bits: CompactDifficulty::try_from_bits(0x2007_ffff).expect("valid nBits"),
            },
        )]
    }
}

/// On-disk header record: `hash(32) ++ prev_hash(32) ++ time(4 LE) ++ bits(4 LE)`
/// = 72 bytes.
#[derive(PersistentRecord)]
pub struct PersistentHeaderValue {
    hash: [u8; 32],
    prev_hash: [u8; 32],
    time: u32,
    bits: u32,
}

impl PersistentRecord for PersistentHeaderValue {
    type Domain = HeaderValue;

    fn from_domain(domain: &HeaderValue) -> Self {
        Self {
            hash: <[u8; 32]>::from(domain.hash),
            prev_hash: <[u8; 32]>::from(domain.prev_hash),
            time: domain.time,
            bits: domain.bits.as_bits(),
        }
    }

    fn into_domain(self) -> Result<HeaderValue, DecodeError> {
        Ok(HeaderValue {
            hash: BlockHash::from(self.hash),
            prev_hash: BlockHash::from(self.prev_hash),
            time: self.time,
            bits: CompactDifficulty::try_from_bits(self.bits)
                .map_err(|_| DecodeError::Invalid("invalid nBits".to_owned()))?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // `RecordLayout` brings the derived `encode`/`decode` into scope for the
    // direct calls below (the free `encode_value`/`decode_value` helpers go
    // through `PersistentRecord`, but these tests exercise the record directly).
    use zaino_persistence_codec::RecordLayout;

    #[test]
    fn header_value_encodes_to_a_pinned_72_byte_layout() {
        let value = HeaderValue {
            hash: BlockHash::from([0x11; 32]),
            prev_hash: BlockHash::from([0x22; 32]),
            time: 0x0403_0201,
            bits: CompactDifficulty::try_from_bits(0x2007_ffff).expect("valid nBits"),
        };
        let mut expected = Vec::new();
        expected.extend_from_slice(&[0x11; 32]);
        expected.extend_from_slice(&[0x22; 32]);
        expected.extend_from_slice(&0x0403_0201u32.to_le_bytes());
        expected.extend_from_slice(&0x2007_ffffu32.to_le_bytes());
        assert_eq!(expected.len(), 72);

        let bytes = PersistentHeaderValue::from_domain(&value).encode();
        assert_eq!(bytes, expected);

        let back = PersistentHeaderValue::decode(&bytes)
            .expect("decode")
            .into_domain()
            .expect("into_domain");
        assert_eq!(back, value);
    }

    #[test]
    fn a_short_header_buffer_is_rejected() {
        assert!(PersistentHeaderValue::decode(&[0u8; 71]).is_err());
        assert!(PersistentHeaderValue::decode(&[0u8; 73]).is_err());
    }
}
