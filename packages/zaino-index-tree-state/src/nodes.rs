//! Per-level node sequences (slot = address, no keys, no framing)
//!
//! - `l00` = every leaf (a leaf sits at either parity); `l01`..`l31` = even index only
//! - odd internal index never retained (an ommer = sibling of a right child = a left child = even)

use incrementalmerkletree::Address;
use zaino_persistence::SequenceRead;
use zaino_primitives::types::ShieldedPool;

use crate::level_table;

/// Bytes per node (all three pool node types serialize to 32)
pub(crate) const NODE: usize = 32;

pub(crate) const MERKLE_DEPTH: u8 = 32;

/// Nodes retained at `level` once the tree holds `size` commitments (= that level's length)
///
/// - level ℓ ≥ 1 retains `(ℓ, i)` for even `i`, from the size it completes at, `(i + 1)·2^ℓ`
/// - the fold stores a node in the batch holding its last leaf (`Frontier::append_batch_visiting`)
/// - the tests' oracle for what a fold retains
#[cfg(test)]
pub(crate) fn retained_nodes(level: u8, size: u64) -> u64 {
    match level {
        0 => size,
        _ => (size >> level).div_ceil(2),
    }
}

/// `(level, slot)`: a node's place in the per-level sequences
pub(crate) type Slot = (u8, u64);

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

/// One pool's tree of `size` commitments in `view`: what
/// [`frontier_at`](crate::fold::frontier_at) walks
#[derive(Debug, Clone, Copy)]
pub(crate) struct NodeView<'a, V> {
    pub(crate) view: &'a V,
    pub(crate) pool: ShieldedPool,
    pub(crate) size: u64,
}

impl<V: SequenceRead> NodeView<'_, V> {
    /// `None` = not retained (a tree of `size` reads only retained nodes: corruption)
    pub(crate) fn get(&self, addr: Address) -> Option<[u8; NODE]> {
        let (level, index) = slot(addr)?;
        let bytes = self.view.record(level_table(self.pool, level), index)?;
        Some(bytes[..].try_into().expect("NODE bytes"))
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
