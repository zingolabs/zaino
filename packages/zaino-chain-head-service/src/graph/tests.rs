//! The [`ChainGraph`] contract, as a suite any implementation must pass.
//!
//! The checks here assert only through the read traits, so they hold whatever
//! the underlying representation is. [`graph_contract!`] instantiates them for a
//! concrete type, and a property test drives random move sequences through the
//! same [`ChainGraph::check_representation`] the per-move invariants rest on.

use zaino_chain_head::{ChainHeadBlock, ChainHeadWork};
use zaino_primitives::types::{
    Block, BlockCommitments, BlockHash, BlockHeader, ChainMetadata, CompactDifficulty,
    EquihashSolution, Height, MerkleRoot, TreeRoots,
};

use crate::graph::ChainGraph;

/// A graph a test can look inside, beyond what the read traits expose.
///
/// Kept out of the production surface: it names what an implementation is
/// holding rather than anything about the chain, and only the contract suite
/// needs it.
pub(crate) trait InspectableGraph: ChainGraph {
    /// Every retained block's hash, canonical and competing alike.
    fn retained_hashes(&self) -> std::collections::HashSet<BlockHash>;

    /// How many blocks are retained in total.
    fn retained_block_count(&self) -> usize;

    /// The first internal invariant that fails, or `Ok` if all hold.
    fn check_representation(&self) -> Result<(), String>;
}

/// A valid nBits value: non-negative, non-zero, no overflow.
fn valid_bits() -> CompactDifficulty {
    CompactDifficulty::try_from_bits(0x2007_ffff).expect("valid nBits")
}

/// A block hash built from a small integer, so chains read by id.
fn test_hash(id: u32) -> BlockHash {
    let mut bytes = [0u8; 32];
    bytes[..4].copy_from_slice(&id.to_le_bytes());
    BlockHash::from(bytes)
}

fn test_height(height: u32) -> Height {
    Height::try_from(height).expect("test height in range")
}

fn empty_tree_roots() -> TreeRoots {
    TreeRoots {
        sapling: None,
        orchard: None,
        ironwood: None,
    }
}

fn raw_block(height: u32, hash: BlockHash, parent_hash: BlockHash) -> Block {
    Block {
        header: BlockHeader {
            hash,
            version: 4,
            prev_hash: parent_hash,
            height: test_height(height),
            time: 0,
            merkle_root: MerkleRoot::from([0; 32]),
            block_commitments: BlockCommitments::from([0; 32]),
            bits: valid_bits(),
            nonce: [0; 32],
            solution: EquihashSolution::Regtest([0; 36]),
        },
        transactions: vec![],
        chain_metadata: ChainMetadata::ZERO,
    }
}

/// A [`ChainHeadBlock`] with an explicit id, parent id, and cumulative work.
fn chain_head_block(height: u32, id: u32, parent_id: u32, work: u128) -> ChainHeadBlock {
    let hash = test_hash(id);
    let parent_hash = test_hash(parent_id);
    ChainHeadBlock {
        reference: zaino_primitives::types::BlockRef {
            hash,
            height: test_height(height),
        },
        parent_hash,
        work: ChainHeadWork::anchored_at(work),
        block: raw_block(height, hash, parent_hash),
        tree_roots: empty_tree_roots(),
    }
}

/// A child of `parent`, one height above it and naming it as its parent.
fn child_of(parent: &ChainHeadBlock, id: u32, work: u128) -> ChainHeadBlock {
    let hash = test_hash(id);
    let height = u32::from(parent.height()).saturating_add(1);
    ChainHeadBlock {
        reference: zaino_primitives::types::BlockRef {
            hash,
            height: test_height(height),
        },
        parent_hash: parent.hash(),
        work: ChainHeadWork::anchored_at(work),
        block: raw_block(height, hash, parent.hash()),
        tree_roots: empty_tree_roots(),
    }
}

/// A graph exercising both moves: a linear spine, then a heavier competing
/// branch laid down after a rewind, leaving canonical and competing blocks
/// retained side by side.
pub(crate) fn reorged_graph<G: ChainGraph>() -> G {
    let mut graph = G::from_initial_block(chain_head_block(0, 0, 0, 10));
    graph
        .extend(chain_head_block(1, 1, 0, 20))
        .expect("child of tip");
    graph
        .extend(chain_head_block(2, 2, 1, 30))
        .expect("child of tip");
    graph
        .extend(chain_head_block(3, 3, 2, 40))
        .expect("child of tip");

    graph
        .rewind_to(zaino_primitives::types::BlockRef {
            hash: test_hash(1),
            height: test_height(1),
        })
        .expect("canonical rewind target");
    graph
        .extend(chain_head_block(2, 12, 1, 35))
        .expect("child of tip");
    graph
        .extend(chain_head_block(3, 13, 12, 45))
        .expect("child of tip");
    graph
        .extend(chain_head_block(4, 14, 13, 55))
        .expect("child of tip");
    graph
}

/// The invariant checks, asserting only through the read traits.
pub(crate) mod contract {
    use zaino_primitives::types::BlockHash;

    use crate::graph::ChainGraph;

    /// The tip is retained, canonical, and agrees with `tip_block`.
    pub(crate) fn check_tip_is_retained_and_canonical<G: ChainGraph>(
        graph: &G,
    ) -> Result<(), String> {
        let tip = graph.best_tip();
        if graph.block_by_hash(&tip.hash).is_none() {
            return Err(format!("tip {} is not retained", tip.hash));
        }
        if !graph.is_on_best_chain(tip) {
            return Err(format!("tip {} is not canonical", tip.hash));
        }
        if graph.tip_block().reference != tip {
            return Err("tip_block disagrees with best_tip".to_string());
        }
        Ok(())
    }

    /// The canonical chain's last block is the tip.
    pub(crate) fn check_best_chain_ends_at_tip<G: ChainGraph>(graph: &G) -> Result<(), String> {
        let last = graph.best_chain().last().map(|block| block.reference);
        if last != Some(graph.best_tip()) {
            return Err(format!(
                "best_chain ends at {last:?}, not tip {:?}",
                graph.best_tip()
            ));
        }
        Ok(())
    }

    /// Canonical heights are consecutive and each block links to its predecessor.
    pub(crate) fn check_best_chain_is_linked<G: ChainGraph>(graph: &G) -> Result<(), String> {
        let mut previous: Option<(u32, BlockHash)> = None;
        for block in graph.best_chain() {
            let height = u32::from(block.height());
            if let Some((prev_height, prev_hash)) = previous {
                if height != prev_height.saturating_add(1) {
                    return Err(format!(
                        "canonical heights {prev_height} and {height} are not consecutive"
                    ));
                }
                if block.parent_hash != prev_hash {
                    return Err(format!(
                        "canonical block at height {height} does not link to its predecessor"
                    ));
                }
            }
            previous = Some((height, block.hash()));
        }
        Ok(())
    }

    /// Every block `best_chain` yields is canonical.
    pub(crate) fn check_best_chain_blocks_are_canonical<G: ChainGraph>(
        graph: &G,
    ) -> Result<(), String> {
        for block in graph.best_chain() {
            if !graph.is_on_best_chain(block.reference) {
                return Err(format!(
                    "best_chain yielded non-canonical block {}",
                    block.hash()
                ));
            }
        }
        Ok(())
    }

    /// The heaviest block is retained and carries at least the tip's work.
    pub(crate) fn check_heaviest_block_is_retained<G: ChainGraph>(graph: &G) -> Result<(), String> {
        let heaviest = graph.heaviest_block();
        if graph.block_by_hash(&heaviest.hash()).is_none() {
            return Err(format!(
                "heaviest block {} is not retained",
                heaviest.hash()
            ));
        }
        if heaviest.work < graph.tip_block().work {
            return Err("heaviest block has less work than the tip".to_string());
        }
        Ok(())
    }

    /// Every invariant, in order; the first failure short-circuits.
    pub(crate) fn representation_invariants<G: ChainGraph>(graph: &G) -> Result<(), String> {
        check_tip_is_retained_and_canonical(graph)?;
        check_best_chain_ends_at_tip(graph)?;
        check_best_chain_is_linked(graph)?;
        check_best_chain_blocks_are_canonical(graph)?;
        check_heaviest_block_is_retained(graph)?;
        Ok(())
    }
}

/// Random move sequences, for the property test.
pub(crate) mod properties {
    use proptest::prelude::*;

    use super::{chain_head_block, child_of, test_height};
    use crate::graph::{tests::InspectableGraph, ChainGraph};

    /// One move against a graph.
    #[derive(Debug, Clone)]
    pub(crate) enum Move {
        /// Append a valid child of the current tip, with a little more work.
        Extend {
            /// Extra work above the tip's, so a later block outweighs an earlier.
            work_delta: u8,
        },
        /// Rewind to the canonical block this many heights below the tip.
        RewindBy(u8),
        /// Trim everything below this many heights beneath the tip.
        Trim(u8),
    }

    /// A strategy generating any single move with small parameters.
    pub(crate) fn a_move() -> impl Strategy<Value = Move> {
        prop_oneof![
            any::<u8>().prop_map(|work_delta| Move::Extend { work_delta }),
            any::<u8>().prop_map(Move::RewindBy),
            any::<u8>().prop_map(Move::Trim),
        ]
    }

    /// Applies a move. Extend appends a fresh unique block; rewind and trim
    /// target retained blocks, and a refused move leaves the graph unchanged.
    fn apply<G: ChainGraph>(graph: &mut G, mv: &Move, next_id: &mut u32) {
        match mv {
            Move::Extend { work_delta } => {
                let child = {
                    let tip = graph.tip_block();
                    let work = tip
                        .work
                        .as_u128()
                        .saturating_add(u128::from(*work_delta).saturating_add(1));
                    child_of(tip, *next_id, work)
                };
                *next_id = next_id.saturating_add(1);
                // A valid child of the current tip, so this always succeeds.
                let _ = graph.extend(child);
            }
            Move::RewindBy(steps) => {
                let tip_height = u32::from(graph.best_tip().height);
                let target_height = tip_height.saturating_sub(u32::from(*steps));
                let target = graph
                    .best_block_by_height(test_height(target_height))
                    .map(|block| block.reference);
                if let Some(target) = target {
                    let _ = graph.rewind_to(target);
                }
            }
            Move::Trim(steps) => {
                let floor = graph.best_tip().height.saturating_sub(u32::from(*steps));
                graph.remove_finalized_blocks(floor);
            }
        }
    }

    /// Applies every move to a fresh graph, asserting the representation holds
    /// before the first move and after each one.
    pub(crate) fn run_moves<G: InspectableGraph>(moves: &[Move]) -> Result<(), String> {
        let mut next_id: u32 = 1_000;
        let mut graph = G::from_initial_block(chain_head_block(0, 0, 0, 1));
        check_retention(&graph)?;
        for mv in moves {
            apply(&mut graph, mv, &mut next_id);
            check_retention(&graph)?;
        }
        Ok(())
    }

    /// The representation holds, and the two retention accessors agree.
    fn check_retention<G: InspectableGraph>(graph: &G) -> Result<(), String> {
        graph.check_representation()?;
        if graph.retained_block_count() != graph.retained_hashes().len() {
            return Err(format!(
                "retained_block_count {} disagrees with retained_hashes {}",
                graph.retained_block_count(),
                graph.retained_hashes().len()
            ));
        }
        Ok(())
    }
}

/// Emits the contract suite for one [`ChainGraph`] implementation.
///
/// Each per-invariant `#[test]` delegates to a generic check, and the property
/// test drives random move sequences through `check_representation`.
macro_rules! graph_contract {
    ($name:ident, $graph:ty) => {
        mod $name {
            #[test]
            fn tip_is_retained_and_canonical() {
                $crate::graph::tests::contract::check_tip_is_retained_and_canonical(
                    &$crate::graph::tests::reorged_graph::<$graph>(),
                )
                .expect("tip is retained and canonical");
            }

            #[test]
            fn best_chain_ends_at_tip() {
                $crate::graph::tests::contract::check_best_chain_ends_at_tip(
                    &$crate::graph::tests::reorged_graph::<$graph>(),
                )
                .expect("best chain ends at the tip");
            }

            #[test]
            fn best_chain_is_linked() {
                $crate::graph::tests::contract::check_best_chain_is_linked(
                    &$crate::graph::tests::reorged_graph::<$graph>(),
                )
                .expect("best chain is linked");
            }

            #[test]
            fn best_chain_blocks_are_canonical() {
                $crate::graph::tests::contract::check_best_chain_blocks_are_canonical(
                    &$crate::graph::tests::reorged_graph::<$graph>(),
                )
                .expect("best chain blocks are canonical");
            }

            #[test]
            fn heaviest_block_is_retained() {
                $crate::graph::tests::contract::check_heaviest_block_is_retained(
                    &$crate::graph::tests::reorged_graph::<$graph>(),
                )
                .expect("heaviest block is retained");
            }

            proptest::proptest! {
                #[test]
                fn random_moves_keep_the_graph_consistent(
                    moves in proptest::collection::vec(
                        $crate::graph::tests::properties::a_move(),
                        1..64,
                    )
                ) {
                    $crate::graph::tests::properties::run_moves::<$graph>(&moves)
                        .map_err(|failure| {
                            proptest::test_runner::TestCaseError::fail(failure)
                        })?;
                }
            }
        }
    };
}

graph_contract!(map_backed_snapshot, crate::snapshot::MapBackedSnapshot);
