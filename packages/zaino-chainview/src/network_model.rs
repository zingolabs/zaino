//! Chain view over simulated zebrads, checked against a naive count after every poll
//!
//! - Nodes mine, relay (longest chain wins: regtest work per block is fixed; first seen on a tie),
//!   fork (`invalidateblock` + `generate`), go unready; endpoints polled in random order
//! - Oracle = each endpoint's chain *as last polled*, full length from genesis

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::Arc;

use proptest::prelude::*;
use zaino_primitives::types::{
    Block, BlockHash, BlockHeader, BlockRef, Height, ReorgDepth, Transaction,
};
use zaino_source::mock::MockChain;

use crate::endpoint::Polled;
use crate::endpoints::EndpointIndex;
use crate::{BelowQuorum, ChainView, Endpoint, EndpointSet};

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
    Ready {
        node: usize,
        ready: bool,
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
            1 => (node(), any::<bool>()).prop_map(|(node, ready)| Move::Ready { node, ready }),
            6 => node().prop_map(|node| Move::Poll { node }),
        ],
        1..48,
    )
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

    /// After every poll: the view's tip + agreers = the naive count, each endpoint's own tip = its
    /// polled chain, the epoch moves iff the tip block does, below quorum = the real shortfall
    #[test]
    fn the_tip_is_the_highest_block_a_majority_of_polled_chains_hold(
        nodes in 1..=MAX_NODES,
        moves in moves(),
    ) {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(run(nodes, moves));
    }
}

/// Unique per mined block (`minted`), height in bytes 0..4, never `BlockHash::ZERO`
fn hash(height: usize, minted: u32) -> BlockHash {
    let mut hash = [0x5a; 32];
    hash[..4].copy_from_slice(&(height as u32).to_le_bytes());
    hash[4..8].copy_from_slice(&minted.to_le_bytes());
    BlockHash::from(hash)
}

fn block(height: usize, own: BlockHash, parent: BlockHash) -> Block {
    Block::new(
        BlockHeader::for_tests(height as u32, own.into(), parent.into(), 0),
        vec![Transaction {
            txid: <[u8; 32]>::from(own).into(),
            transparent: Default::default(),
            sprout: Default::default(),
            sapling: Default::default(),
            orchard: Default::default(),
            ironwood: Default::default(),
        }],
    )
}

struct Node {
    mock: Arc<MockChain>,
    /// Its best chain, genesis first
    best: Vec<BlockHash>,
    ready: bool,
}

/// What the view last read from one endpoint
#[derive(Debug, Clone)]
struct Seen {
    chain: Vec<BlockHash>,
    votes: bool,
}

struct Sim {
    nodes: Vec<Node>,
    blocks: HashMap<BlockHash, Block>,
    minted: u32,
}

impl Sim {
    fn mine(&mut self, node: usize, count: u32) {
        for _ in 0..count {
            self.minted += 1;
            let best = &mut self.nodes[node].best;
            let (height, parent) = (best.len(), *best.last().expect("genesis"));
            let own = hash(height, self.minted);
            self.blocks.insert(own, block(height, own, parent));
            best.push(own);
        }
    }

    /// Mock re-served from genesis (a reorg is just a different best chain)
    fn serve(&self, node: usize) {
        let node = &self.nodes[node];
        node.mock.rewind_to(Height::GENESIS);
        node.mock.extend_best(node.best[1..].iter().map(|own| self.blocks[own].clone()));
    }
}

/// Highest block ≥ `threshold` voters hold in their last `DEPTH + 1` blocks, + its holders; and
/// the largest holder group of any block
fn count(seen: &[Option<Seen>], threshold: usize) -> (Option<(BlockRef, EndpointSet)>, usize) {
    let mut holders: HashMap<BlockRef, EndpointSet> = HashMap::new();
    for (index, seen) in seen.iter().enumerate() {
        let Some(seen) = seen.as_ref().filter(|seen| seen.votes) else { continue };
        let window = seen.chain.len().saturating_sub(DEPTH as usize + 1);
        for (height, hash) in seen.chain.iter().enumerate().skip(window) {
            let height = Height::try_from(height as u32).expect("small");
            let voter = EndpointIndex::new(index).expect("< MAX_NODES");
            holders.entry(BlockRef { hash: *hash, height }).or_default().insert(voter);
        }
    }
    let largest = holders.values().map(EndpointSet::count).max().unwrap_or(0);
    let tip = holders
        .into_iter()
        .filter(|(_, held_by)| held_by.count() >= threshold)
        .max_by_key(|(block, _)| block.height);
    (tip, largest)
}

async fn run(n: usize, moves: Vec<Move>) {
    let genesis: Vec<BlockHash> = (0..4).map(|height| hash(height, 0)).collect();
    let mut sim = Sim { nodes: Vec::new(), blocks: HashMap::new(), minted: 0 };
    for (height, own) in genesis.iter().enumerate() {
        let parent = height.checked_sub(1).map_or(BlockHash::ZERO, |below| genesis[below]);
        sim.blocks.insert(*own, block(height, *own, parent));
    }
    for index in 0..n {
        let mock = Arc::new(MockChain::new());
        sim.nodes.push(Node { mock, best: genesis.clone(), ready: true });
        sim.serve(index);
    }
    let depth = ReorgDepth::new(NonZeroU32::new(DEPTH).expect("nz"));
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
    let threshold = reader.quorum().threshold();
    let mut seen: Vec<Option<Seen>> = vec![None; n];
    let (mut tip_was, mut epoch_was) = (None, reader.current().epoch());

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
            Move::Ready { node, ready } => {
                sim.nodes[node % n].ready = ready;
                sim.nodes[node % n].mock.set_ready(ready);
            }
            Move::Poll { node } => {
                let node = node % n;
                let polled = pollers[node].tick().await.expect("a mock never fails transport");
                let chain = sim.nodes[node].best.clone();
                seen[node] = match polled {
                    Polled::Syncing => seen[node].take().map(|seen| Seen { votes: false, ..seen }),
                    _ => Some(Seen { chain, votes: true }),
                };

                let context = format!("step {step} poll {node}: {seen:?}");
                let pinned = reader.current();
                let (expected, largest) = count(&seen, threshold);
                let tip = pinned.tip().map(|tip| (tip.block, tip.agreed_by));
                assert_eq!(tip, expected, "{context}");
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
                let moved = pinned.epoch() != epoch_was;
                assert_eq!(
                    moved,
                    block != tip_was,
                    "{context}: epoch moves iff the tip block does"
                );
                if expected.is_none() {
                    let refused = BelowQuorum { agreeing: largest, threshold, configured: n };
                    assert_eq!(pinned.mempool().err(), Some(refused), "{context}");
                }
                (tip_was, epoch_was) = (block, pinned.epoch());
            }
        }
    }
}
