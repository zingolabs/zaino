//! `subtrees.dat`: 36 B per completed subtree (root, completing height)
//!
//! - slot = subtree index → a `start_index` resume = a seek, not a scan

use zaino_persistence::{
    pages::{PagedFile, Pages, Sealed},
    StoreError,
};
use zaino_primitives::types::{Height, TreeRoot};

pub(crate) const ENTRY: usize = 36;

/// Stored form of a completed subtree (completing hash = that height's `heights.idx` record)
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
        end_height: Height::try_from(height).expect("a sealed entry holds a protocol height"),
    }
}

/// Committed subtree entries of one pool
#[derive(Debug, Clone)]
pub(crate) struct Subtrees {
    entries: Pages,
}

impl Subtrees {
    pub(crate) fn count(&self) -> u64 {
        (self.entries.len() / ENTRY) as u64
    }

    pub(crate) fn get(&self, index: u64) -> SubtreeEntry {
        let at = usize::try_from(index).expect("subtree index fits usize") * ENTRY;
        decode(self.entries.read(at..at + ENTRY).try_into().expect("ENTRY bytes"))
    }
}

/// Writer side: one append-only file per pool + its last seal
#[derive(Debug)]
pub(crate) struct SubtreeFile {
    file: PagedFile,
    sealed: Sealed,
}

impl SubtreeFile {
    /// `file` opened at `sealed`
    pub(crate) fn new(file: PagedFile, sealed: Sealed) -> Self {
        Self { file, sealed }
    }

    /// Appends entry `index` (subtrees complete in order: asserted)
    pub(crate) fn put(&mut self, index: u64, root: &SubtreeEntry) -> Result<(), StoreError> {
        assert_eq!(self.file.len(), index * ENTRY as u64, "subtree {index} appends at the end");
        Ok(self.file.append(&encode(root))?)
    }

    pub(crate) fn seal(&mut self) -> Result<Sealed, StoreError> {
        self.sealed = self.file.seal()?;
        Ok(self.sealed)
    }

    /// Remaps at the last seal, keeping `previous`'s checked pages
    pub(crate) fn snapshot(&self, previous: Option<&Subtrees>) -> Result<Subtrees, StoreError> {
        let entries = self.file.pages(self.sealed, previous.map(|old| &old.entries))?;
        Ok(Subtrees { entries })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Slot = subtree index → a width change re-addresses every entry
    #[test]
    fn subtree_entry_golden_bytes_round_trip() {
        let entry = SubtreeEntry {
            root: TreeRoot::from([0x5a; 32]),
            end_height: Height::try_from(0x0012_3456).expect("in range"),
        };

        let mut expected = [0u8; ENTRY];
        expected[..32].copy_from_slice(&[0x5a; 32]);
        expected[32..].copy_from_slice(&[0x56, 0x34, 0x12, 0x00]);

        assert_eq!(encode(&entry), expected);
        assert_eq!(decode(&expected), entry);
    }
}
