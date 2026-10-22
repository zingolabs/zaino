//! Chain view over simulated zebrads through the real balancer, checked against a naive model
//! after every poll
//!
//! - Nodes mine, relay (longest chain wins: regtest work per block fixed, first seen on a tie),
//!   fork (`invalidateblock` + `generate`), go unreachable
//! - node's poll woken in random order (others poll on their own cadence too: every observation
//!   counts)
//! - Paused clock: the sim stands still while any poll runs (moves land between waits)
//! - Header chain = a real one, fed each polled chain (header sync's part, without its I/O)
//! - Oracle = each trusted validator's chain *as last polled*, full length from genesis, and the
//!   best it was asked under

use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use proptest::prelude::*;
use tokio_util::sync::CancellationToken;
use zaino_header_chain::testing::insert;
use zaino_header_chain::HeaderChain;
use zaino_primitives::types::{BlockRef, FinalityDepth};
use zaino_source::testing::{within, MockChain, MockValidator};
use zaino_traffic::{Health, Limits, Push, TrafficBalancer, TrustedValidator, TrustedValidatorId};
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::BlockHash;

use crate::{ChainView, TrustedValidatorSet};

const DEPTH: u32 = 3;
const MAX_NODES: usize = 5;

/// - `Mine` = `count` blocks on `node`'s own tip
/// - `Relay` = `to` adopts `from`'s chain iff longer
/// - `Fork` = top `drop` blocks invalidated, `mine` mined on the rest (`mine < drop` = retreat)
#[derive(Debug, Clone)]
enum Move {
    Mine { node: usize, count: u32 },
    Relay { from: usize, to: usize },
    Fork { node: usize, drop: u32, mine: u32 },
    Reachable { node: usize, reachable: bool },
    Poll { node: usize },
}

/// Every `node` < `nodes`
fn moves(nodes: usize) -> impl Strategy<Value = Vec<Move>> {
    let node = move || 0..nodes;
    prop::collection::vec(
        prop_oneof![
            3 => (node(), 1u32..=3).prop_map(|(node, count)| Move::Mine { node, count }),
            4 => (node(), node()).prop_map(|(from, to)| Move::Relay { from, to }),
            1 => (node(), 1u32..=DEPTH + 2, 0u32..=DEPTH + 2)
                .prop_map(|(node, drop, mine)| Move::Fork { node, drop, mine }),
            1 => (node(), any::<bool>())
                .prop_map(|(node, reachable)| Move::Reachable { node, reachable }),
            6 => node().prop_map(|node| Move::Poll { node }),
        ],
        1..48,
    )
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

    /// After every poll, best = the header chain's best
    ///
    /// - holders ⊆ reporting validators whose polled chain holds it, ⊇ those claiming it or
    ///   polled under it
    /// - no holder = no mempool
    /// - each trusted validator's own tip = its polled chain's (none once `Down`)
    #[test]
    fn the_tip_is_the_verified_best_and_its_holders_are_the_validators_asked_holding_it(
        (nodes, moves) in (1..=MAX_NODES).prop_flat_map(|nodes| (Just(nodes), moves(nodes))),
    ) {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .expect("runtime")
            .block_on(run(nodes, moves))?;
    }
}

struct Node {
    validator: MockValidator,
    tip: BlockRef,
}

/// What the view last read from one trusted validator, and the best it asked under
#[derive(Clone)]
struct Seen {
    chain: Vec<BlockHash>,
    reporting: bool,
    down: bool,
    under: Option<BlockRef>,
}

/// Its polled tip, never the whole chain (a failure message stays one screen)
impl std::fmt::Debug for Seen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Seen")
            .field("tip_height", &self.chain.len().saturating_sub(1))
            .field("tip", &self.chain.last())
            .field("reporting", &self.reporting)
            .field("down", &self.down)
            .field("under", &self.under)
            .finish()
    }
}

/// `chain` = every block any node mined (one tree, real headers)
struct Sim {
    nodes: Vec<Node>,
    chain: MockChain,
}

impl Sim {
    /// `count` blocks on `parent`, `node` following the new tip (reorg = another tip)
    fn mine(&mut self, node: usize, parent: BlockRef, count: u32) {
        let tip = self.chain.branch(parent).mine_empty(count).tip();
        self.follow(node, tip);
    }

    fn follow(&mut self, node: usize, tip: BlockRef) {
        let node = &mut self.nodes[node];
        node.tip = tip;
        node.validator.follow(&self.chain, tip);
    }

    /// Genesis ..= `node`'s tip
    fn path(&self, node: usize) -> Vec<BlockHash> {
        let blocks = self.chain.blocks(self.nodes[node].tip);
        blocks.iter().map(|block| block.header().hash).collect()
    }
}

/// (must hold, may hold) `best`: claim = it or asked under it ⊆ holders ⊆ polled chain holds it
fn holders(seen: &[Option<Seen>], best: BlockRef) -> (TrustedValidatorSet, TrustedValidatorSet) {
    let (mut must, mut may) = (TrustedValidatorSet::default(), TrustedValidatorSet::default());
    for (index, seen) in seen.iter().enumerate() {
        let Some(seen) = seen.as_ref().filter(|seen| seen.reporting) else { continue };
        let index = TrustedValidatorId::new(index).expect("< MAX_NODES");
        if seen.chain.get(u32::from(best.height) as usize) == Some(&best.hash) {
            may.insert(index);
            if seen.chain.last() == Some(&best.hash) || seen.under == Some(best) {
                must.insert(index);
            }
        }
    }
    (must, may)
}

async fn run(n: usize, moves: Vec<Move>) -> Result<(), TestCaseError> {
    let mut chain = MockChain::regtest();
    let shared = chain.mine_empty(3);
    let depth = FinalityDepth::new(NonZeroU32::new(DEPTH).expect("nz"));
    let mut headers = HeaderChain::anchored(&chain, depth);
    let nodes = (0..n).map(|_| Node { validator: chain.validator(shared), tip: shared });
    let mut sim = Sim { nodes: nodes.collect(), chain };
    let limits = Limits::new(8).expect("8 ≥ MIN_CONNECTIONS");
    let trusted = sim.nodes.iter().enumerate().map(|(index, node)| TrustedValidator {
        address: format!("v{index}"),
        source: Arc::new(node.validator.client()),
        priority: 0,
        limits,
    });
    let (balancer, driver) = TrafficBalancer::new(trusted.collect(), None);
    let schedule = sim.chain.schedule().upgrades;
    let shutdown = tokio_util::sync::CancellationToken::new();
    let view = ChainView::new(balancer.clone(), depth, schedule, shutdown);
    let cancel = CancellationToken::new();
    tokio::spawn(driver.run(cancel.clone()));
    tokio::spawn(view.observation_merge().run(cancel.clone()));
    let validator = |node: usize| TrustedValidatorId::new(node).expect("< MAX_NODES");
    let mut observed: Vec<_> = (0..n).map(|node| balancer.observe(validator(node))).collect();
    let reader = view.subscriber();
    let mut seen: Vec<Option<Seen>> = vec![None; n];

    for (step, next) in moves.into_iter().enumerate() {
        match next {
            Move::Mine { node, count } => sim.mine(node, sim.nodes[node].tip, count),
            Move::Relay { from, to } => {
                let from = sim.nodes[from].tip;
                if from.height > sim.nodes[to].tip.height {
                    sim.follow(to, from);
                }
            }
            Move::Fork { node, drop, mine } => {
                let tip = sim.nodes[node].tip;
                let kept = u32::from(tip.height).saturating_sub(drop);
                let base = sim.chain.ancestor(tip, BlockHeight::from(kept));
                sim.mine(node, base, mine);
            }
            Move::Reachable { node, reachable } => sim.nodes[node].validator.down(!reachable),
            Move::Poll { node } => {
                let under = reader.current().best();
                balancer.pushed(validator(node), Push::Changed);
                within("the node polled", observed[node].changed()).await.expect("driver running");
                observed[node].mark_changed();
                // every merge of what landed (paused clock: runs before the timer fires)
                tokio::time::sleep(Duration::from_millis(1)).await;
                for (index, watch) in observed.iter_mut().enumerate() {
                    if !watch.has_changed().expect("driver running") {
                        continue;
                    }
                    let observation = watch.borrow_and_update().clone().expect("polled");
                    let chain = sim.path(index);
                    let down = observation.health == Health::Down;
                    seen[index] = match observation.polled {
                        Ok(_) => Some(Seen { chain, reporting: true, down, under }),
                        Err(_) => {
                            seen[index].take().map(|seen| Seen { reporting: false, down, ..seen })
                        }
                    };
                    // header sync's part: what was read, verified (most work wins, tie = first)
                    if let Some(read) = seen[index].as_ref().filter(|seen| seen.reporting) {
                        let height = BlockHeight::from(read.chain.len() as u32 - 1);
                        let tip = BlockRef { hash: *read.chain.last().expect("genesis"), height };
                        let _side_or_known = insert(&mut headers, &sim.chain.blocks(tip));
                    }
                }
                view.set_chain(headers.snapshot());

                let best = headers.snapshot().map(|snapshot| snapshot.best());
                let context = format!("move {step}, poll of {node}, best {best:?}, seen {seen:?}");
                let pinned = reader.current();
                prop_assert_eq!(
                    pinned.best(),
                    best,
                    "view's best vs the header chain's: {}",
                    context
                );
                let held = pinned.held_by();
                match best {
                    Some(best) => {
                        let (must, may) = holders(&seen, best);
                        prop_assert!(
                            held.covers(must),
                            "holders {:?} miss a validator claiming or asked under best {:?}: {}",
                            held,
                            must,
                            context
                        );
                        prop_assert!(
                            may.covers(held),
                            "holders {:?} outside those whose polled chain holds best {:?}: {}",
                            held,
                            may,
                            context
                        );
                    }
                    None => {
                        prop_assert!(
                            held.is_empty(),
                            "nothing verified, yet holders {:?}: {}",
                            held,
                            context
                        );
                    }
                }
                let served = pinned.mempool().is_some();
                prop_assert_eq!(
                    served,
                    !held.is_empty(),
                    "mempool served iff a holder {:?}: {}",
                    held,
                    context
                );
                for (index, seen) in seen.iter().enumerate() {
                    let theirs = seen.as_ref().filter(|seen| !seen.down).and_then(|seen| {
                        let height = BlockHeight::from(seen.chain.len() as u32 - 1);
                        Some(BlockRef { hash: *seen.chain.last()?, height })
                    });
                    prop_assert_eq!(
                        pinned.trusted_validators()[index].tip(),
                        theirs,
                        "validator {}'s shown tip vs its polled chain's (none once down): {}",
                        index,
                        context
                    );
                }
            }
        }
    }
    cancel.cancel();
    Ok(())
}
