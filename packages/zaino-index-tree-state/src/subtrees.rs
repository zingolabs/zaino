//! `<pool>/subtrees`: 36 B per completed subtree (root, completing height)
//!
//! - slot = subtree index → a `start_index` resume = a seek, not a scan

use zaino_persistence::SequenceRead;
use zaino_primitives::types::{Height, ShieldedPool, TreeRoot};

use crate::subtree_table;

pub(crate) const ENTRY: usize = 36;

/// Stored form of a completed subtree (completing hash = that height's `heights` record)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SubtreeEntry {
    pub(crate) root: TreeRoot,
    pub(crate) end_height: Height,
}

/// root ‖ end_height (u32 little-endian)
pub(crate) fn encode(entry: &SubtreeEntry) -> [u8; ENTRY] {
    let mut bytes = [0u8; ENTRY];
    bytes[..32].copy_from_slice(&<[u8; 32]>::from(entry.root));
    bytes[32..].copy_from_slice(&u32::from(entry.end_height).to_le_bytes());
    bytes
}

pub(crate) fn decode(bytes: &[u8; ENTRY]) -> SubtreeEntry {
    let height = u32::from_le_bytes(*bytes[32..].first_chunk::<4>().expect("ENTRY = 32 + 4"));
    SubtreeEntry {
        root: TreeRoot::from(*bytes.first_chunk::<32>().expect("ENTRY > 32")),
        end_height: Height::try_from(height).expect("a committed entry holds a protocol height"),
    }
}

/// `pool`'s entry `index` (`None` = at or past the count)
pub(crate) fn entry(
    view: &impl SequenceRead,
    pool: ShieldedPool,
    index: u64,
) -> Option<SubtreeEntry> {
    let bytes = view.record(subtree_table(pool), index)?;
    Some(decode(bytes[..].try_into().expect("ENTRY bytes")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Slot = subtree index → a width change re-addresses every entry
    #[test]
    fn subtree_entry_golden_bytes_round_trip() {
        let entry = SubtreeEntry {
            root: TreeRoot::from([0x5a; 32]),
            end_height: Height::try_from(0x0012_3456u32).expect("in range"),
        };

        let mut expected = [0u8; ENTRY];
        expected[..32].copy_from_slice(&[0x5a; 32]);
        expected[32..].copy_from_slice(&[0x56, 0x34, 0x12, 0x00]);

        assert_eq!(encode(&entry), expected);
        assert_eq!(decode(&expected), entry);
    }
}
