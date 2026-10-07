//! `heights`: 48 B per height (hash, time, the three cumulative tree sizes)

use zaino_primitives::types::{BlockHash, BlockTime, PerPool, TreeSize, TreeSizes};

/// Bytes per record (slot = height)
pub(crate) const RECORD: usize = 48;

/// Block identity + tree sizes after it
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TreeStateHeight {
    pub(crate) hash: BlockHash,
    pub(crate) time: BlockTime,
    pub(crate) sizes: TreeSizes,
}

impl TreeStateHeight {
    /// Sizes as positions (the node sequences' arithmetic)
    pub(crate) fn positions(&self) -> PerPool<u64> {
        self.sizes.map(u64::from)
    }
}

/// hash ‖ time ‖ sapling ‖ orchard ‖ ironwood (u32s little-endian)
pub(crate) fn encode(height: &TreeStateHeight) -> [u8; RECORD] {
    let mut bytes = [0u8; RECORD];
    bytes[..32].copy_from_slice(&<[u8; 32]>::from(height.hash));
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
        hash: BlockHash::from(*bytes.first_chunk::<32>().expect("RECORD > 32")),
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
        let height = TreeStateHeight {
            hash: BlockHash::from([0xab; 32]),
            time: 0x1122_3344,
            sizes: TreeSizes {
                sapling: TreeSize::from(1),
                orchard: TreeSize::from(0x0100_0000),
                ironwood: TreeSize::from(u32::MAX),
            },
        };

        let mut expected = Vec::with_capacity(RECORD);
        expected.extend_from_slice(&[0xab; 32]);
        expected.extend_from_slice(&[0x44, 0x33, 0x22, 0x11]);
        expected.extend_from_slice(&[0x01, 0x00, 0x00, 0x00]);
        expected.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        expected.extend_from_slice(&[0xff, 0xff, 0xff, 0xff]);

        let encoded = encode(&height);
        assert_eq!(encoded.as_slice(), expected.as_slice());
        assert_eq!(decode(&encoded), height);
    }
}
