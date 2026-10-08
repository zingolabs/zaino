//! The subtree-roots index key and value, and their on-disk records.
//!
//! The key is the subtree index — a `u32` big-endian, so byte order is numeric
//! order and the `WalkOrdered` namespace walks subtrees in ascending index. The
//! value is a completed subtree's [`SubtreeRoot`]: the 32-byte root in internal
//! (unreversed) order, plus the height whose block completed it (the
//! `end_height` `z_getsubtreesbyindex` reports). The [`EntryCodec`] is generic
//! over the [`Pool`], since every pool's subtree namespace shares this format
//! and differs only in identity.

use zaino_persistence_codec::layout::{BeU32, BeU64, Cursor, LayoutAtom, Writer};
use zaino_persistence_codec::{DecodeError, EntryCodec, KeyOrder, PersistentRecord, RecordLayout};
use zaino_sync::primitives::BlockHeight;

use super::pool::Pool;
use super::SubtreesIndex;

/// A completed subtree's root and the height that completed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtreeRoot {
    /// The level-[`SUBTREE_LEVEL`](super::pool::SUBTREE_LEVEL) node, in internal
    /// (unreversed) byte order — the orientation every pool stores and serves.
    pub root: [u8; 32],
    /// The height of the block that completed this subtree (`z_getsubtreesbyindex`
    /// `end_height`).
    pub completing_height: BlockHeight,
}

/// On-disk record for a [`SubtreeRoot`]: the 32 root bytes verbatim, then the
/// completing height as a big-endian `u64`.
pub struct PersistentSubtreeRoot {
    root: [u8; 32],
    completing_height: u64,
}

impl RecordLayout for PersistentSubtreeRoot {
    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::with_capacity(40);
        writer.bytes32(&self.root);
        BeU64(self.completing_height).encode(&mut writer);
        writer.into_bytes()
    }

    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut cursor = Cursor::new(bytes);
        let root = cursor.bytes32()?;
        let completing_height = BeU64::decode(&mut cursor)?.0;
        cursor.finish()?;
        Ok(Self {
            root,
            completing_height,
        })
    }
}

impl PersistentRecord for PersistentSubtreeRoot {
    type Domain = SubtreeRoot;

    fn from_domain(domain: &SubtreeRoot) -> Self {
        Self {
            root: domain.root,
            completing_height: domain.completing_height.into(),
        }
    }

    fn into_domain(self) -> Result<SubtreeRoot, DecodeError> {
        Ok(SubtreeRoot {
            root: self.root,
            completing_height: BlockHeight::from(self.completing_height),
        })
    }
}

/// On-disk record for the subtree-index key: a big-endian `u32`, so the key's
/// byte order is its numeric (walk) order.
pub struct PersistentSubtreeKey(u32);

impl RecordLayout for PersistentSubtreeKey {
    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::with_capacity(4);
        BeU32(self.0).encode(&mut writer);
        writer.into_bytes()
    }

    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut cursor = Cursor::new(bytes);
        let raw = BeU32::decode(&mut cursor)?.0;
        cursor.finish()?;
        Ok(Self(raw))
    }
}

impl PersistentRecord for PersistentSubtreeKey {
    type Domain = u32;

    fn from_domain(domain: &u32) -> Self {
        Self(*domain)
    }

    fn into_domain(self) -> Result<u32, DecodeError> {
        Ok(self.0)
    }
}

impl<P: Pool> EntryCodec for SubtreesIndex<P> {
    type Key = u32;
    type Value = SubtreeRoot;
    type PersistentKey = PersistentSubtreeKey;
    type PersistentValue = PersistentSubtreeRoot;

    const KEY_ORDER: KeyOrder = KeyOrder::WalkOrdered;

    fn fingerprint_samples() -> Vec<(u32, SubtreeRoot)> {
        // The format is pool-independent; the samples exercise the key's four
        // bytes and the value's root-plus-height layout.
        vec![
            (
                0,
                SubtreeRoot {
                    root: [0u8; 32],
                    completing_height: BlockHeight::new(0),
                },
            ),
            (
                0x0102_0304,
                SubtreeRoot {
                    root: [0xAB; 32],
                    completing_height: BlockHeight::new(558_822),
                },
            ),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::super::pool::SaplingPool;
    use super::*;
    use zaino_persistence_codec::{decode_key, decode_value, encode_key, encode_value};

    type Idx = SubtreesIndex<SaplingPool>;

    #[test]
    fn key_is_four_big_endian_bytes() {
        let bytes = encode_key::<Idx>(&0x0102_0304);
        assert_eq!(bytes, vec![1, 2, 3, 4]);
        assert_eq!(decode_key::<Idx>(&bytes).expect("decode"), 0x0102_0304);
    }

    #[test]
    fn keys_sort_in_numeric_order() {
        // Byte order is the subtree walk order, so a byte-comparing backend
        // stores subtree 2 after subtree 1 after subtree 0.
        let encoded: Vec<Vec<u8>> = [0u32, 1, 2, 255, 256, 65_536, u32::MAX]
            .iter()
            .map(encode_key::<Idx>)
            .collect();
        let mut sorted = encoded.clone();
        sorted.sort();
        assert_eq!(encoded, sorted);
    }

    #[test]
    fn value_round_trips() {
        let value = SubtreeRoot {
            root: [0x5a; 32],
            completing_height: BlockHeight::new(780_364),
        };
        let bytes = encode_value::<Idx>(&value);
        assert_eq!(bytes.len(), 40, "32 root bytes + 8 height bytes");
        assert_eq!(decode_value::<Idx>(&bytes).expect("decode"), value);
    }

    #[test]
    fn a_truncated_value_is_rejected() {
        let value = SubtreeRoot {
            root: [1u8; 32],
            completing_height: BlockHeight::new(1),
        };
        let bytes = encode_value::<Idx>(&value);
        assert!(decode_value::<Idx>(&bytes[..bytes.len() - 1]).is_err());
    }
}
