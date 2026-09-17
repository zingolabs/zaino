//! Random move sequences on [`MapBackedSnapshot`], checked against a model.
//!
//! The model is the plain reading of each move: the canonical chain is a list
//! that grows by extension and shrinks by rewinding or trimming, and the
//! retained set grows only by extension and shrinks only by trimming. After
//! every move the snapshot must agree with the model and pass its own
//! invariant check. Refused moves must leave the snapshot unchanged.

use std::collections::{HashMap, HashSet};

use proptest::{prelude::*, test_runner::TestCaseError};
use zaino_chain_head::ChainHeadSnapshot as _;
use zaino_primitives::types::{rpc::ChainTipStatus, BlockHash, BlockRef};

use super::chain_head_block;
use crate::{
    graph::{ChainGraph as _, NotChildOfTip, NotOnBestChain},
    snapshot::MapBackedSnapshot,
    tests::{best_chain_hashes, hash, height},
};

/// Height of the first block, high enough that rewinds and trims stay above
/// genesis.
const BASE_HEIGHT: u32 = 100;

#[derive(Debug, Clone)]
enum Move {
    /// A child of the tip carrying `work` more than the tip.
    Extend { work: u8 },
    /// A retained child of the tip off the best chain, as when a reorg returns
    /// to a branch it displaced. A no-op when there is none.
    ExtendRetained { pick: u8 },
    /// A block that does not attach: its parent is not retained, or its parent
    /// is the tip but its height is not the next one.
    ExtendDetached {
        unknown_parent: bool,
        height_choice: u8,
    },
    /// To the canonical block `depth` below the tip, wrapping at the bottom.
    Rewind { depth: u8 },
    /// To a retained block off the best chain, or to one never retained.
    RewindOffChain { pick: u8 },
    /// Trims at `tip + 4 - shift`: from four above the tip to fifteen below.
    Trim { shift: u8 },
}

fn a_move() -> impl Strategy<Value = Move> {
    prop_oneof![
        4 => (1u8..=8).prop_map(|work| Move::Extend { work }),
        2 => any::<u8>().prop_map(|pick| Move::ExtendRetained { pick }),
        1 => (any::<bool>(), 0u8..4).prop_map(|(unknown_parent, height_choice)| {
            Move::ExtendDetached {
                unknown_parent,
                height_choice,
            }
        }),
        2 => any::<u8>().prop_map(|depth| Move::Rewind { depth }),
        1 => any::<u8>().prop_map(|pick| Move::RewindOffChain { pick }),
        1 => (0u8..20).prop_map(|shift| Move::Trim { shift }),
    ]
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    height: u32,
    parent: u16,
    work: u128,
}

/// What the snapshot should hold, by block id.
struct Model {
    /// The canonical chain, lowest first.
    chain: Vec<u16>,
    retained: HashMap<u16, Entry>,
    next_id: u16,
}

impl Model {
    const FIRST_ID: u16 = 1;

    fn new() -> Self {
        Self {
            chain: vec![Self::FIRST_ID],
            retained: HashMap::from([(
                Self::FIRST_ID,
                Entry {
                    height: BASE_HEIGHT,
                    parent: 0,
                    work: 1,
                },
            )]),
            next_id: Self::FIRST_ID + 1,
        }
    }

    fn fresh_id(&mut self) -> u16 {
        let id = self.next_id;
        self.next_id = id.checked_add(1).expect("test ids fit u16");
        id
    }

    fn tip_id(&self) -> u16 {
        *self.chain.last().expect("the model chain is never empty")
    }

    fn entry(&self, id: u16) -> Entry {
        self.retained[&id]
    }

    fn reference(&self, id: u16) -> BlockRef {
        BlockRef {
            hash: hash(id),
            height: height(self.entry(id).height),
        }
    }
}

/// What a refused move must leave untouched.
fn fingerprint(graph: &MapBackedSnapshot) -> (BlockRef, Vec<BlockHash>, HashSet<BlockHash>) {
    (
        graph.best_tip(),
        best_chain_hashes(graph),
        graph.retained_hashes(),
    )
}

fn apply(
    graph: &mut MapBackedSnapshot,
    model: &mut Model,
    step: &Move,
) -> Result<(), TestCaseError> {
    let tip_id = model.tip_id();
    let tip = model.entry(tip_id);
    match *step {
        Move::Extend { work } => {
            let id = model.fresh_id();
            let child = Entry {
                height: tip.height + 1,
                parent: tip_id,
                work: tip.work + u128::from(work),
            };
            let block = chain_head_block(child.height, id, tip_id, child.work);
            prop_assert_eq!(graph.extend(block), Ok(()));
            model.chain.push(id);
            model.retained.insert(id, child);
        }
        Move::ExtendRetained { pick } => {
            let mut children: Vec<u16> = model
                .retained
                .iter()
                .filter(|(_, entry)| entry.parent == tip_id && entry.height == tip.height + 1)
                .map(|(id, _)| *id)
                .collect();
            if children.is_empty() {
                return Ok(());
            }
            children.sort_unstable();
            let id = children[usize::from(pick) % children.len()];
            let child = model.entry(id);
            let block = chain_head_block(child.height, id, child.parent, child.work);
            prop_assert_eq!(graph.extend(block), Ok(()));
            model.chain.push(id);
        }
        Move::ExtendDetached {
            unknown_parent,
            height_choice,
        } => {
            let id = model.fresh_id();
            let (parent, block_height) = if unknown_parent {
                (model.fresh_id(), tip.height + 1)
            } else {
                let block_height = match height_choice {
                    0 => tip.height,
                    1 => tip.height + 2,
                    2 => tip.height + 3,
                    _ => tip.height - 1,
                };
                (tip_id, block_height)
            };
            let block = chain_head_block(block_height, id, parent, tip.work + 1);
            let refused = NotChildOfTip {
                tip: model.reference(tip_id),
                block: block.reference,
            };
            let before = fingerprint(graph);
            prop_assert_eq!(graph.extend(block), Err(refused));
            prop_assert_eq!(fingerprint(graph), before);
        }
        Move::Rewind { depth } => {
            let below_tip = usize::from(depth) % model.chain.len();
            let index = model.chain.len() - 1 - below_tip;
            let target = model.reference(model.chain[index]);
            prop_assert_eq!(graph.rewind_to(target), Ok(()));
            model.chain.truncate(index + 1);
        }
        Move::RewindOffChain { pick } => {
            let canonical: HashSet<u16> = model.chain.iter().copied().collect();
            let mut off_chain: Vec<u16> = model
                .retained
                .keys()
                .copied()
                .filter(|id| !canonical.contains(id))
                .collect();
            off_chain.sort_unstable();
            let target = match off_chain.len() {
                0 => BlockRef {
                    hash: hash(model.fresh_id()),
                    height: height(tip.height),
                },
                len => model.reference(off_chain[usize::from(pick) % len]),
            };
            let before = fingerprint(graph);
            prop_assert_eq!(graph.rewind_to(target), Err(NotOnBestChain));
            prop_assert_eq!(fingerprint(graph), before);
        }
        Move::Trim { shift } => {
            let floor = if shift <= 4 {
                tip.height + u32::from(4 - shift)
            } else {
                tip.height.saturating_sub(u32::from(shift - 4))
            };
            graph.remove_finalized_blocks(height(floor));
            model
                .retained
                .retain(|id, entry| entry.height >= floor || *id == tip_id);
            let retained = &model.retained;
            model.chain.retain(|id| retained.contains_key(id));
        }
    }
    Ok(())
}

fn check(graph: &MapBackedSnapshot, model: &Model) -> Result<(), TestCaseError> {
    graph.check_invariants().map_err(TestCaseError::fail)?;

    let tip_id = model.tip_id();
    prop_assert_eq!(graph.best_tip(), model.reference(tip_id));
    let chain: Vec<BlockHash> = model.chain.iter().map(|id| hash(*id)).collect();
    prop_assert_eq!(best_chain_hashes(graph), chain);
    let retained: HashSet<BlockHash> = model.retained.keys().map(|id| hash(*id)).collect();
    prop_assert_eq!(graph.retained_hashes(), retained.clone());
    prop_assert_eq!(graph.retained_block_count(), model.retained.len());

    let max_work = model
        .retained
        .values()
        .map(|entry| entry.work)
        .max()
        .expect("the model always retains its tip");
    let heaviest = graph.heaviest_block();
    prop_assert_eq!(heaviest.work.as_u128(), max_work);
    if model.entry(tip_id).work == max_work {
        prop_assert_eq!(heaviest.hash(), hash(tip_id));
    }

    let tips = graph.chain_tips();
    let active: Vec<BlockHash> = tips
        .iter()
        .filter(|tip| tip.status == ChainTipStatus::Active)
        .map(|tip| tip.hash)
        .collect();
    prop_assert_eq!(active, vec![hash(tip_id)]);
    for tip in &tips {
        prop_assert!(
            retained.contains(&tip.hash),
            "chain tip {} is not retained",
            tip.hash
        );
    }
    Ok(())
}

proptest! {
    #[test]
    fn random_moves_keep_the_snapshot_consistent(
        moves in prop::collection::vec(a_move(), 1..64)
    ) {
        let mut model = Model::new();
        let first = chain_head_block(BASE_HEIGHT, Model::FIRST_ID, 0, 1);
        let mut graph = MapBackedSnapshot::from_initial_block(first);
        check(&graph, &model)?;

        for (index, step) in moves.iter().enumerate() {
            let context = |error: TestCaseError| {
                TestCaseError::fail(format!("move {index} {step:?}: {error}"))
            };
            apply(&mut graph, &mut model, step).map_err(context)?;
            check(&graph, &model).map_err(context)?;
        }
    }
}
