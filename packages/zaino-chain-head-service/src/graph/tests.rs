//! The [`ChainGraph`] contract, run against every implementation.
//!
//! The suite has three parts, all generic over the graph type:
//!
//! - [`checks`]: example tests, one move each.
//! - [`properties`]: random move sequences, compared with a simple model after
//!   every move.
//! - [`contract`]: the [`ChainGraph`] invariants, checked through the read
//!   traits alone. The property test runs it after every move.
//!
//! To join the suite, an implementation provides [`InspectableGraph`] in its
//! own test module and adds one [`graph_contract!`] line.

use std::collections::HashSet;

use zaino_chain_head::{ChainHeadBlock, ChainHeadWork};
use zaino_primitives::types::{BlockHash, BlockRef, TreeRoots};

use super::ChainGraph;
use crate::tests::block;

mod checks;
mod contract;
mod properties;

/// What the suite needs from inside an implementation.
///
/// A requirement, not a helper: implementing it adds nothing to the type. It
/// gives the suite what the read traits cannot, and it exists only in test
/// builds. Each implementation provides it in its own test module, where its
/// private fields are visible.
pub(crate) trait InspectableGraph: ChainGraph {
    /// Every retained block's hash, the tip included.
    ///
    /// The property test compares this set with its model, and uses it to
    /// confirm that a refused move changed nothing. The read traits have no
    /// way to list every retained block.
    fn retained_hashes(&self) -> HashSet<BlockHash>;

    /// How many blocks the implementation holds.
    ///
    /// Compared with the model, so a block held twice is caught even without a
    /// representation check.
    fn retained_block_count(&self) -> usize;

    /// The first invariant of this representation that does not hold, if any.
    ///
    /// The property test runs it after every move, so the implementation's
    /// internal invariants are exercised by every random sequence without a
    /// generator of its own. It checks only what the read traits cannot show:
    /// [`contract`] checks the [`ChainGraph`] invariants for every
    /// implementation.
    fn check_representation(&self) -> Result<(), String>;
}

/// A retained block carrying `work`, independent of its parent's.
fn chain_head_block(h: u32, id: u16, parent: u16, work: u128) -> ChainHeadBlock {
    let block = block(h, id, parent);
    ChainHeadBlock {
        reference: BlockRef {
            hash: block.header.hash,
            height: block.header.height,
        },
        parent_hash: block.header.prev_hash,
        work: ChainHeadWork::anchored_at(work),
        block,
        tree_roots: TreeRoots {
            sapling: None,
            orchard: None,
            ironwood: None,
        },
    }
}

/// Declares the contract tests for one implementation, in a module `$name`.
///
/// A macro because a `#[test]` function cannot be generic: each implementation
/// needs its own named test items, each calling the generic check.
macro_rules! graph_contract {
    ($name:ident, $graph:ty) => {
        mod $name {
            use super::{checks, properties};

            #[test]
            fn extending_with_a_block_whose_parent_is_not_the_tip_is_refused() {
                checks::extending_with_a_block_whose_parent_is_not_the_tip_is_refused::<$graph>();
            }

            #[test]
            fn extending_with_a_block_at_the_wrong_height_is_refused() {
                checks::extending_with_a_block_at_the_wrong_height_is_refused::<$graph>();
            }

            #[test]
            fn trimming_above_the_tip_keeps_the_tip() {
                checks::trimming_above_the_tip_keeps_the_tip::<$graph>();
            }

            #[test]
            fn rewinding_keeps_the_old_tip_as_a_competing_block() {
                checks::rewinding_keeps_the_old_tip_as_a_competing_block::<$graph>();
            }

            #[test]
            fn rewinding_to_a_competing_block_is_refused() {
                checks::rewinding_to_a_competing_block_is_refused::<$graph>();
            }

            #[test]
            fn a_heavier_competing_block_is_the_heaviest() {
                checks::a_heavier_competing_block_is_the_heaviest::<$graph>();
            }

            #[test]
            fn the_tip_wins_an_equal_work_tie() {
                checks::the_tip_wins_an_equal_work_tie::<$graph>();
            }

            proptest::proptest! {
                #[test]
                fn random_moves_keep_the_graph_consistent(
                    moves in proptest::collection::vec(properties::a_move(), 1..64)
                ) {
                    properties::random_moves::<$graph>(&moves)?;
                }
            }
        }
    };
}

graph_contract!(map_backed_snapshot, crate::snapshot::MapBackedSnapshot);
