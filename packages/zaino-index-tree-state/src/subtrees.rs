//! `<pool>/subtrees`: 36 B per completed subtree (root, completing height)
//!
//! - slot = subtree index → a `start_index` resume = a seek, not a scan

use zaino_persistence::SequenceRead;
use zaino_primitives::types::TreeRoot;
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::ShieldedPool;

use crate::tables::subtree_table;

pub(crate) const ENTRY: usize = 36;

/// Stored form of a completed subtree (completing hash = that height's `heights` record)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SubtreeEntry {
    pub(crate) root: TreeRoot,
    pub(crate) end_height: BlockHeight,
}

/// root ‖ end_height (u32 big-endian: a locator)
pub(crate) fn encode(entry: &SubtreeEntry) -> [u8; ENTRY] {
    let mut bytes = [0u8; ENTRY];
    bytes[..32].copy_from_slice(&entry.root.0);
    bytes[32..].copy_from_slice(&u32::from(entry.end_height).to_be_bytes());
    bytes
}

pub(crate) fn decode(bytes: &[u8; ENTRY]) -> SubtreeEntry {
    let height = u32::from_be_bytes(*bytes[32..].first_chunk::<4>().expect("ENTRY = 32 + 4"));
    SubtreeEntry {
        root: TreeRoot(*bytes.first_chunk::<32>().expect("ENTRY > 32")),
        end_height: BlockHeight::from(height),
    }
}

/// `pool`'s entry `index` (`None` = at or past the count)
pub(crate) fn entry(
    view: &impl SequenceRead,
    pool: ShieldedPool,
    index: u64,
) -> Option<SubtreeEntry> {
    let bytes = view.sequence(subtree_table(pool)).record(index)?;
    Some(decode(bytes[..].try_into().expect("ENTRY bytes")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Slot = subtree index → a width change re-addresses every entry
    #[test]
    fn subtree_entry_golden_bytes_round_trip() {
        let root: [u8; 32] = std::array::from_fn(|at| 0xe0 ^ at as u8);
        let entry =
            SubtreeEntry { root: TreeRoot(root), end_height: BlockHeight::from(0x0012_3456) };
        // height = a locator: big-endian
        #[rustfmt::skip]
        let fields = [
            ("root",       "e0e1e2e3e4e5e6e7e8e9eaebecedeeeff0f1f2f3f4f5f6f7f8f9fafbfcfdfeff"),
            ("end_height", "00123456"),
        ];

        let encoded = encode(&entry);

        let hex = |bytes: &[u8]| bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
        let mut offset = 0;
        for (field, expected) in fields {
            let end = offset + expected.len() / 2;
            assert_eq!(hex(&encoded[offset..end]), expected, "{field} at bytes {offset}..{end}");
            offset = end;
        }
        assert_eq!(offset, ENTRY, "fields fill the entry, nothing trailing");
        assert_eq!(decode(&encoded), entry, "decode inverts encode");
    }
}
