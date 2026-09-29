//! Per-level node files (fixed stride: offset = address, no keys, no framing)
//!
//! - `l00.dat` = every leaf (a leaf sits at either parity); `l01.dat`..`l31.dat` = even index only
//! - odd internal index never retained (an ommer = sibling of a right child = a left child = even)

use incrementalmerkletree::Address;
use zaino_persistence::{
    pages::{PagedFile, Pages, Sealed},
    StoreError,
};

/// Bytes per node (all three pool node types serialize to 32)
pub(crate) const NODE: usize = 32;

pub(crate) const MERKLE_DEPTH: u8 = 32;

/// Nodes retained at `level` once the tree holds `size` commitments (= that file's length in nodes)
///
/// - level ℓ ≥ 1 retains `(ℓ, i)` for even `i`, from the size it completes at, `(i + 1)·2^ℓ`
/// - the fold stores a node in the batch holding its last leaf (`Frontier::append_batch_visiting`)
pub(crate) fn retained_nodes(level: u8, size: u64) -> u64 {
    match level {
        0 => size,
        _ => (size >> level).div_ceil(2),
    }
}

/// `(level, slot)`: a node's place in the per-level files (same key in the nonfinalised tier)
pub(crate) type Slot = (u8, u64);

/// Materialised by a fold, not yet fsynced
pub(crate) type NonFinalizedNodes = imbl::OrdMap<Slot, [u8; NODE]>;

/// `None` = nothing retained at `addr`
pub(crate) fn slot(addr: Address) -> Option<Slot> {
    let level = u8::from(addr.level());
    let index = addr.index();

    match level {
        0 => Some((0, index)),
        1..=31 if index.is_multiple_of(2) => Some((level, index / 2)),
        _ => None,
    }
}

/// One pool's tree of `size` commitments, nonfinalised nodes over durable: what
/// [`frontier_at`](crate::fold::frontier_at) walks
#[derive(Debug, Clone, Copy)]
pub(crate) struct NodeView<'a> {
    pub(crate) non_finalized: &'a NonFinalizedNodes,
    pub(crate) durable: &'a PoolNodes,
    pub(crate) size: u64,
}

impl NodeView<'_> {
    /// `None` = not retained (a tree of `size` reads only retained nodes: corruption)
    pub(crate) fn get(&self, addr: Address) -> Option<[u8; NODE]> {
        let slot = slot(addr)?;
        self.non_finalized.get(&slot).copied().or_else(|| self.durable.get(slot))
    }
}

/// Committed nodes for one pool, one view per level of exactly its retained length
#[derive(Debug, Default)]
pub(crate) struct PoolNodes {
    levels: Vec<Pages>,
}

impl PoolNodes {
    /// Durable node at `slot`, `None` past what the committed size retains
    pub(crate) fn get(&self, (level, index): Slot) -> Option<[u8; NODE]> {
        let at = usize::try_from(index).ok()?.checked_mul(NODE)?;
        let pages = self.levels.get(usize::from(level))?;
        (at + NODE <= pages.len()).then(|| pages.read(at..at + NODE).try_into().expect("NODE"))
    }
}

/// `l{level:02}.dat`
pub(crate) fn level_file(level: u8) -> String {
    format!("l{level:02}.dat")
}

/// Writer side: the 32 files + which grew since the last seal
///
/// - fsync linear in file count (3.3 ms for 1, 357.8 ms for 100, measured)
/// - `seal()` skips clean levels (~4 dirty per batch); remapping 100 files = 235 µs
#[derive(Debug)]
pub(crate) struct NodeFiles {
    levels: Vec<PagedFile>,
    sealed: [Sealed; MERKLE_DEPTH as usize],
    dirty: [bool; MERKLE_DEPTH as usize],
}

impl NodeFiles {
    /// `levels` = `l00.dat`..`l31.dat`, opened at `sealed`
    pub(crate) fn new(levels: Vec<PagedFile>, sealed: [Sealed; MERKLE_DEPTH as usize]) -> Self {
        assert_eq!(levels.len(), usize::from(MERKLE_DEPTH), "one file per level");
        Self { levels, sealed, dirty: [false; MERKLE_DEPTH as usize] }
    }

    /// Appends `node` at `slot` (slot = pure function of position; each level grows in order)
    pub(crate) fn put(
        &mut self,
        (level, index): Slot,
        node: &[u8; NODE],
    ) -> Result<(), StoreError> {
        let at = usize::from(level);
        assert_eq!(self.levels[at].len(), index * NODE as u64, "({level}, {index}) not at end");
        self.levels[at].append(node)?;
        self.dirty[at] = true;
        Ok(())
    }

    /// Seals every level that grew (fsync); every level's seal, for the manifest
    pub(crate) fn seal(&mut self) -> Result<[Sealed; MERKLE_DEPTH as usize], StoreError> {
        for (level, file) in self.levels.iter_mut().enumerate() {
            if self.dirty[level] {
                self.sealed[level] = file.seal()?;
            }
        }
        self.dirty = [false; MERKLE_DEPTH as usize];
        Ok(self.sealed)
    }

    /// Remaps every level at its seal, keeping `previous`'s checked pages
    pub(crate) fn snapshot(&self, previous: Option<&PoolNodes>) -> Result<PoolNodes, StoreError> {
        let mut levels = Vec::with_capacity(self.levels.len());
        for (level, file) in self.levels.iter().enumerate() {
            let old = previous.and_then(|old| old.levels.get(level));
            levels.push(file.pages(self.sealed[level], old)?);
        }
        Ok(PoolNodes { levels })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use incrementalmerkletree::Level;
    use incrementalmerkletree::Position;

    /// Retained counts agree with what the fold actually materialises, and an address round-trips
    /// to the slot the counts imply.
    #[test]
    fn retained_counts_match_the_nodes_a_fold_would_materialise() {
        // Level 0 keeps every leaf; higher levels keep one node per even index.
        assert_eq!(retained_nodes(0, 0), 0);
        assert_eq!(retained_nodes(0, 7), 7);
        assert_eq!(retained_nodes(5, 0), 0);

        // Each even node lands at the size that completes it: (1,0) at 2, (1,2) at 6, (2,0) at 4,
        // (2,2) at 12, (3,0) at 8.
        for (level, size, expected) in [
            (1u8, 1u64, 0u64),
            (1, 2, 1),
            (1, 5, 1),
            (1, 6, 2),
            (2, 3, 0),
            (2, 4, 1),
            (2, 11, 1),
            (2, 12, 2),
            (3, 7, 0),
            (3, 8, 1),
        ] {
            assert_eq!(retained_nodes(level, size), expected, "level {level} at size {size}");
        }

        // A leaf addresses its own position; an even internal index halves; an odd one is absent.
        assert_eq!(slot(Address::from(Position::from(9))), Some((0u8, 9u64)));
        assert_eq!(slot(Address::from_parts(Level::from(1), 4)), Some((1, 2)));
        assert_eq!(slot(Address::from_parts(Level::from(1), 5)), None);
        assert_eq!(slot(Address::from_parts(Level::from(32), 0)), None);
    }
}
