//! Chain view over simulated zebrads, checked against a naive model after every poll
//!
//! - Nodes mine, relay (longest chain wins: regtest work per block is fixed; first seen on a tie),
//!   fork (`invalidateblock` + `generate`), go unreachable; endpoints polled in random order
//! - Header chain = a real one, fed each polled chain (header sync's part, without its I/O)
//! - Oracle = each endpoint's chain *as last polled*, full length from genesis, and the best it
//!   was asked under

use std::num::NonZeroU32;
use std::sync::Arc;

use proptest::prelude::*;
use zaino_header_chain::HeaderChain;
use zaino_primitives::testing::Chain;
use zaino_primitives::types::{BlockHash, BlockRef, Height, ReorgDepth};
use zaino_source::mock::MockChain;

use crate::endpoints::EndpointIndex;
use crate::{ChainView, Endpoint, EndpointSet, Unserved};

const DEPTH: u32 = 3;
const MAX_NODES: usize = 5;

#[derive(Debug, Clone)]
enum Move {
    /// `count` new blocks on `node`'s own tip
    Mine {
        node: usize,
        count: u32,
    },
    /// `to` hears `from`'s chain: adopts it iff longer
    Relay {
        from: usize,
        to: usize,
    },
    /// Top `drop` blocks invalidated, `mine` mined on what is left (`mine < drop` = a retreat)
    Fork {
        node: usize,
        drop: u32,
        mine: u32,
    },
    Reachable {
        node: usize,
        reachable: bool,
    },
    Poll {
        node: usize,
    },
}

fn moves() -> impl Strategy<Value = Vec<Move>> {
    let node = || 0..MAX_NODES;
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

    /// After every poll, with the header chain's best as the tip: its holders ⊆ the reporting
    /// validators whose polled chain holds it, ⊇ those whose claim is it or whose poll asked under
    /// it; no holder = no tip (`NotHeld`); each endpoint's own tip = its polled chain's; the epoch
    /// moves iff the tip block does
    #[test]
    fn the_tip_is_the_verified_best_and_its_holders_are_the_validators_asked_holding_it(
        nodes in 1..=MAX_NODES,
        moves in moves(),
    ) {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(run(nodes, moves));
    }
}

struct Node {
    mock: Arc<MockChain>,
    /// Its best chain, genesis first
    best: Vec<BlockHash>,
}

/// What the view last read from one endpoint, and the best it asked under
#[derive(Debug, Clone)]
struct Seen {
    chain: Vec<BlockHash>,
    reporting: bool,
    under: Option<BlockRef>,
}

/// `chain` = every block any node mined (one tree, real headers)
struct Sim {
    nodes: Vec<Node>,
    chain: Chain,
}

impl Sim {
    fn mine(&mut self, node: usize, count: u32) {
        let best = &mut self.nodes[node].best;
        for _ in 0..count {
            best.push(self.chain.mine(*best.last().expect("genesis")).hash);
        }
    }

    /// Mock re-served from genesis, genesis included (a zebrad always holds it; a reorg is just a
    /// different best chain)
    fn serve(&self, node: usize) {
        let node = &self.nodes[node];
        node.mock.extend_best(node.best.iter().map(|own| self.chain.block(*own).clone()));
    }
}

/// (must hold, may hold) `best`: claim = it or asked under it ⊆ holders ⊆ polled chain holds it
fn holders(seen: &[Option<Seen>], best: BlockRef) -> (EndpointSet, EndpointSet) {
    let (mut must, mut may) = (EndpointSet::default(), EndpointSet::default());
    for (index, seen) in seen.iter().enumerate() {
        let Some(seen) = seen.as_ref().filter(|seen| seen.reporting) else { continue };
        let index = EndpointIndex::new(index).expect("< MAX_NODES");
        if seen.chain.get(u32::from(best.height) as usize) == Some(&best.hash) {
            may.insert(index);
            if seen.chain.last() == Some(&best.hash) || seen.under == Some(best) {
                must.insert(index);
            }
        }
    }
    (must, may)
}

async fn run(n: usize, moves: Vec<Move>) {
    let mut chain = Chain::new();
    let shared = chain.extend(chain.genesis().hash, 3);
    let genesis: Vec<BlockHash> = chain.path(shared.hash).iter().map(|b| b.header().hash).collect();
    let depth = ReorgDepth::new(NonZeroU32::new(DEPTH).expect("nz"));
    let mut headers = HeaderChain::regtest_in_memory(chain.genesis().hash, depth);
    let mut sim = Sim { nodes: Vec::new(), chain };
    for index in 0..n {
        let mock = Arc::new(MockChain::new());
        sim.nodes.push(Node { mock, best: genesis.clone() });
        sim.serve(index);
    }
    let endpoints = sim
        .nodes
        .iter()
        .enumerate()
        .map(|(index, node)| Endpoint {
            address: format!("v{index}"),
            source: Arc::clone(&node.mock),
        })
        .collect();
    let (view, pollers) = ChainView::new(endpoints, depth).expect("1..=5 endpoints");
    let reader = view.subscriber();
    let mut seen: Vec<Option<Seen>> = vec![None; n];
    let (mut tip_was, mut epoch_was) = (None, reader.tail().ok());

    for (step, next) in moves.into_iter().enumerate() {
        match next {
            Move::Mine { node, count } => {
                sim.mine(node % n, count);
                sim.serve(node % n);
            }
            Move::Relay { from, to } => {
                let (from, to) = (from % n, to % n);
                if sim.nodes[from].best.len() > sim.nodes[to].best.len() {
                    sim.nodes[to].best = sim.nodes[from].best.clone();
                    sim.serve(to);
                }
            }
            Move::Fork { node, drop, mine } => {
                let node = node % n;
                let best = &mut sim.nodes[node].best;
                best.truncate(best.len().saturating_sub(drop as usize).max(1));
                sim.mine(node, mine);
                sim.serve(node);
            }
            Move::Reachable { node, reachable } => {
                sim.nodes[node % n].mock.set_reachable(reachable)
            }
            Move::Poll { node } => {
                let node = node % n;
                let chain = sim.nodes[node].best.clone();
                let under = reader.current().best();
                seen[node] = match pollers[node].tick().await {
                    Ok(_) => Some(Seen { chain, reporting: true, under }),
                    Err(failed) => {
                        assert!(
                            !pollers[node].failed(&failed, 1),
                            "one failure = degraded, not down"
                        );
                        seen[node].take().map(|seen| Seen { reporting: false, ..seen })
                    }
                };

                // header sync's part: what was read, verified (most work wins, tie = first seen)
                if let Some(read) = seen[node].as_ref().filter(|seen| seen.reporting) {
                    let tip = *read.chain.last().expect("genesis");
                    let _side_or_known = headers.insert_blocks(&sim.chain.path(tip));
                }
                view.set_verified(headers.verified());

                let best = headers.verified().map(|verified| verified.best());
                let context = format!("step {step} poll {node} best {best:?}: {seen:?}");
                let pinned = reader.current();
                assert_eq!(pinned.best(), best, "{context}");
                let tip = pinned.tip().map(|tip| (tip.block, tip.held_by));
                match (best, tip) {
                    (Some(best), Some((block, held))) => {
                        let (must, may) = holders(&seen, best);
                        assert_eq!(block, best, "{context}: the tip = the verified best");
                        assert!(held.covers(must), "{context}: {held:?} ⊇ {must:?}");
                        assert!(may.covers(held), "{context}: {held:?} ⊆ {may:?}");
                    }
                    (Some(best), None) => {
                        let (must, _) = holders(&seen, best);
                        assert!(must.is_empty(), "{context}: {must:?} hold it, no tip");
                    }
                    (None, tip) => assert_eq!(tip, None, "{context}: nothing verified, no tip"),
                }
                for (index, seen) in seen.iter().enumerate() {
                    let theirs = seen.as_ref().and_then(|seen| {
                        let height = Height::try_from(seen.chain.len() as u32 - 1).expect("small");
                        Some(BlockRef { hash: *seen.chain.last()?, height })
                    });
                    assert_eq!(
                        pinned.endpoints()[index].tip(),
                        theirs,
                        "{context}: endpoint {index}"
                    );
                }
                let block = tip.map(|(block, _)| block);
                let epoch = reader.tail();
                let moved = match (&epoch_was, &epoch) {
                    (Some(was), Ok(now)) => !was.same_epoch(now),
                    (None, Err(_)) => false,
                    _ => true,
                };
                assert_eq!(
                    moved,
                    block != tip_was,
                    "{context}: the feed opens a new epoch iff the tip block moves"
                );
                if tip.is_none() {
                    let refused = match best {
                        None => Unserved::NoBestTip,
                        Some(best) => {
                            Unserved::NotHeld { height: u32::from(best.height), configured: n }
                        }
                    };
                    assert_eq!(pinned.mempool().err(), Some(refused), "{context}");
                    assert_eq!(epoch.as_ref().err(), Some(&refused), "{context}: feed refuses too");
                }
                (tip_was, epoch_was) = (block, epoch.ok());
            }
        }
    }
}
