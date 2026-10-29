//! `heights`: 48 B per height (hash, time, the three cumulative tree sizes)

use zaino_primitives::types::{TreeSize, TreeSizes};
use zcash_protocol::BlockHash;

/// Bytes per record (slot = height)
pub(crate) const RECORD: usize = 48;

/// Block identity + tree sizes after it
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TreeStateHeight {
    pub(crate) hash: BlockHash,
    pub(crate) time: u32,
    pub(crate) sizes: TreeSizes,
}

/// hash ‖ time ‖ sapling ‖ orchard ‖ ironwood (u32s little-endian)
pub(crate) fn encode(height: &TreeStateHeight) -> [u8; RECORD] {
    let mut bytes = [0u8; RECORD];
    bytes[..32].copy_from_slice(&height.hash.0);
    for (at, value) in [
        (32, height.time),
        (36, u32::from(height.sizes.sapling)),
        (40, u32::from(height.sizes.orchard)),
        (44, u32::from(height.sizes.ironwood)),
    ] {
        bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }
    bytes
}

pub(crate) fn decode(bytes: &[u8; RECORD]) -> TreeStateHeight {
    let u32_at = |at: usize| {
        u32::from_le_bytes(*bytes[at..].first_chunk::<4>().expect("RECORD = 32 + 4 × 4"))
    };
    TreeStateHeight {
        hash: BlockHash(*bytes.first_chunk::<32>().expect("RECORD > 32")),
        time: u32_at(32),
        sizes: TreeSizes {
            sapling: TreeSize::from(u32_at(36)),
            orchard: TreeSize::from(u32_at(40)),
            ironwood: TreeSize::from(u32_at(44)),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Slot = height × RECORD → a width or order change silently re-addresses every stored height
    #[test]
    fn height_record_golden_bytes_round_trip() {
        let hash: [u8; 32] = std::array::from_fn(|at| u8::try_from(at).expect("32 < 256"));
        let height = TreeStateHeight {
            hash: BlockHash(hash),
            time: 0x1122_3344,
            sizes: TreeSizes {
                sapling: TreeSize::from(1),
                orchard: TreeSize::from(0x0100_0000),
                ironwood: TreeSize::from(u32::MAX),
            },
        };
        // quantities little-endian, hash as serialized
        #[rustfmt::skip]
        let fields = [
            ("hash",     "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"),
            ("time",     "44332211"),
            ("sapling",  "01000000"),
            ("orchard",  "00000001"),
            ("ironwood", "ffffffff"),
        ];

        let encoded = encode(&height);

        let hex = |bytes: &[u8]| bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
        let mut offset = 0;
        for (field, expected) in fields {
            let end = offset + expected.len() / 2;
            assert_eq!(hex(&encoded[offset..end]), expected, "{field} at bytes {offset}..{end}");
            offset = end;
        }
        assert_eq!(offset, RECORD, "fields fill the record, nothing trailing");
        assert_eq!(decode(&encoded), height, "decode inverts encode");
    }
}
