//! HeadersIndex (BlockLocal × Append): height → (hash, prev_hash, time, bits).

use zaino_persistence_codec::{DecodeError, EntryCodec};
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

    fn encode_key(key: &BlockHeight) -> Vec<u8> {
        key.value().to_le_bytes().to_vec()
    }

    fn encode_value(value: &HeaderValue) -> Vec<u8> {
        let mut buf = Vec::with_capacity(72);
        buf.extend_from_slice(&<[u8; 32]>::from(value.hash));
        buf.extend_from_slice(&<[u8; 32]>::from(value.prev_hash));
        buf.extend_from_slice(&value.time.to_le_bytes());
        buf.extend_from_slice(&value.bits.as_bits().to_le_bytes());
        buf
    }

    fn decode_key(bytes: &[u8]) -> Result<BlockHeight, DecodeError> {
        let arr: [u8; 8] = bytes
            .try_into()
            .map_err(|_| DecodeError::Invalid(format!("expected 8 bytes, got {}", bytes.len())))?;
        Ok(BlockHeight::new(u64::from_le_bytes(arr)))
    }

    fn decode_value(bytes: &[u8]) -> Result<HeaderValue, DecodeError> {
        if bytes.len() != 72 {
            return Err(DecodeError::Invalid(format!(
                "expected 72 bytes, got {}",
                bytes.len()
            )));
        }
        let mut hash = [0u8; 32];
        let mut prev_hash = [0u8; 32];
        hash.copy_from_slice(&bytes[0..32]);
        prev_hash.copy_from_slice(&bytes[32..64]);
        // The 72-byte length is checked above, so each fixed-offset window below
        // is exactly 4 bytes and the `try_into` array conversions cannot fail.
        // PLAN: replace this manual offset arithmetic with a fixed-layout decode
        // helper (a cursor / typed field reader) that removes these guarded
        // `expect`s entirely.
        Ok(HeaderValue {
            hash: BlockHash::from(hash),
            prev_hash: BlockHash::from(prev_hash),
            time: u32::from_le_bytes(bytes[64..68].try_into().expect("4 bytes")),
            bits: CompactDifficulty::try_from_bits(u32::from_le_bytes(
                bytes[68..72].try_into().expect("4 bytes"),
            ))
            .map_err(|_| DecodeError::Invalid("invalid nBits".to_owned()))?,
        })
    }
}
