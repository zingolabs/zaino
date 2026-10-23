//! Per-level node sequences (slot = address, no keys, no framing)
//!
//! - `l00` = every leaf (a leaf sits at either parity)
//! - `l01`..`l31` = even index only
//! - odd internal index never retained (an ommer = sibling of a right child = a left child = even)

use incrementalmerkletree::Address;
use zcash_primitives::merkle_tree::HashSer;

/// Bytes per node (all three pool node types serialize to 32)
pub(crate) const NODE: usize = 32;

pub(crate) const MERKLE_DEPTH: u8 = 32;

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

pub(crate) fn encode<H: HashSer>(node: &H) -> [u8; NODE] {
    let mut bytes = [0u8; NODE];
    node.write(&mut bytes[..]).expect("a commitment tree node serializes to exactly 32 bytes");
    bytes
}

/// `None` = non-canonical field element
pub(crate) fn decode<H: HashSer>(bytes: &[u8; NODE]) -> Option<H> {
    H::read(&bytes[..]).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use incrementalmerkletree::Level;
    use incrementalmerkletree::Position;

    /// Leaf = own position, even internal index halved, odd internal + root = none
    #[test]
    fn a_node_address_maps_to_its_retained_slot() {
        #[rustfmt::skip]
        let cases = [
            // (level, index, slot)
            (0u8, 9u64, Some((0u8, 9u64))),
            (1,   4,    Some((1, 2))),
            (1,   5,    None),
            (32,  0,    None),
        ];
        for (level, index, expected) in cases {
            let addr = Address::from_parts(Level::from(level), index);
            assert_eq!(slot(addr), expected, "level {level} index {index}");
        }
        let leaf = Address::from(Position::from(9));
        assert_eq!(slot(leaf), Some((0, 9)), "a position's leaf address = the level 0 slot");
    }
}
