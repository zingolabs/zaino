//! `Holders` against a naive oracle (V1, V2): random validator histories, answered as zebrad
//! answers, `check()` after every step
//!
//! - world = one block tree (`testing::Chain`, real headers), each validator a path in it
//! - ours = a real `HeaderChain` fed whole validator chains (regtest: most work = longest)
//! - oracle = each validator's whole chain at every moment it answered since its last poll
//! - swarm: whole step kinds switched off per case

use std::num::NonZeroU32;
use std::sync::Arc;

use proptest::prelude::*;
use zaino_header_chain::{HeaderChain, VerifiedChain};
use zaino_primitives::testing::Chain;
use zaino_primitives::types::{BlockHash, BlockRef, Height, ReorgDepth};

use super::Holders;
use crate::endpoints::{Agreement, EndpointIndex};

const DEPTH: u32 = 3;
const VALIDATORS: usize = 4;

/// - `Relay` = `to` adopts `from`'s chain iff longer; `Fork` = top `drop` invalidated, `mine`
///   mined on the rest (`mine < drop` = retreat)
/// - header sync: `Learn` verifies `node`'s chain; `Finalize` = the boundary (holders never
///   consulted: the driver's call); `Serve` = a run off `node`'s best, ending `below` its tip
#[derive(Debug, Clone)]
enum Step {
    Mine { node: usize, count: u32 },
    Relay { from: usize, to: usize },
    Fork { node: usize, drop: u32, mine: u32 },
    Learn { node: usize },
    Finalize,
    Poll { node: usize, answer: Answer },
    Serve { node: usize, below: u32 },
}

/// - `Partial` = one `getblockhash` item failed
/// - `Raced` = reorged between `getblockchaininfo` and the `getblockhash` items
/// - `Failed` = timeout / transport: whole poll lost
#[derive(Debug, Clone, Copy)]
enum Answer {
    Full,
    Partial { dropped: usize },
    Raced { drop: u32, mine: u32 },
    Failed,
}

fn steps() -> impl Strategy<Value = (u8, Vec<Step>)> {
    let node = || 0..VALIDATORS;
    let answer = prop_oneof![
        4 => Just(Answer::Full),
        1 => (0usize..2).prop_map(|dropped| Answer::Partial { dropped }),
        1 => (0u32..=2, 0u32..=3).prop_map(|(drop, mine)| Answer::Raced { drop, mine }),
        1 => Just(Answer::Failed),
    ];
    let step = prop_oneof![
        3 => (node(), 1u32..=3).prop_map(|(node, count)| Step::Mine { node, count }),
        3 => (node(), node()).prop_map(|(from, to)| Step::Relay { from, to }),
        1 => (node(), 1u32..=DEPTH + 2, 0u32..=DEPTH + 2)
            .prop_map(|(node, drop, mine)| Step::Fork { node, drop, mine }),
        3 => node().prop_map(|node| Step::Learn { node }),
        1 => Just(Step::Finalize),
        6 => (node(), answer).prop_map(|(node, answer)| Step::Poll { node, answer }),
        2 => (node(), 0u32..=DEPTH + 1).prop_map(|(node, below)| Step::Serve { node, below }),
    ];
    (any::<u8>(), prop::collection::vec(step, 1..64))
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// After every step
    ///
    /// - holders ⊆ validators whose chain held the block when they answered (never stale), ⊇
    ///   those a fresh full poll found holding the boundary or the best; none if lost / silent
    /// - agreement = naive §7 classification when the poll is fresh + whole; claim-only always
    #[test]
    fn holders_and_agreement_answer_like_the_naive_oracle(
        nodes in 1..=VALIDATORS,
        (off, steps) in steps(),
    ) {
        run(nodes, off, steps);
    }
}

/// One validator as the oracle knows it, since its last poll
///
/// - `moments` = its whole chain at each answer (poll, race, served runs), the poll's first
/// - `under` = the best the poll asked under; `whole` = every item answered, no race, no run since
#[derive(Debug, Clone)]
enum Known {
    Silent,
    Polled { claim: BlockRef, moments: Vec<Vec<BlockHash>>, under: Option<BlockRef>, whole: bool },
}

fn tip(path: &[BlockHash]) -> BlockRef {
    let height = Height::try_from(path.len() as u32 - 1).expect("small");
    BlockRef { hash: *path.last().expect("genesis held"), height }
}

fn holds(path: &[BlockHash], block: BlockRef) -> bool {
    path.get(u32::from(block.height) as usize) == Some(&block.hash)
}

fn run(n: usize, off: u8, steps: Vec<Step>) {
    assert!((1..=VALIDATORS).contains(&n), "model: 1..=VALIDATORS validators");
    let depth = ReorgDepth::new(NonZeroU32::new(DEPTH).expect("nz"));
    let mut world = Chain::new();
    let genesis = world.genesis().hash;
    let mut nodes: Vec<Vec<BlockHash>> = vec![vec![genesis]; n];
    let mut ours = HeaderChain::regtest_in_memory(genesis, depth);
    let mut holders = Holders::new(n, depth);
    let mut known = vec![Known::Silent; n];
    let mut verified: Option<VerifiedChain> = None;

    let mine = |world: &mut Chain, path: &mut Vec<BlockHash>, count: u32| {
        for _ in 0..count {
            path.push(world.mine(*path.last().expect("genesis held")).hash);
        }
    };
    let fork = |world: &mut Chain, path: &mut Vec<BlockHash>, drop: u32, count: u32| {
        path.truncate(path.len().saturating_sub(drop as usize).max(1));
        mine(world, path, count);
    };

    for (at, step) in steps.into_iter().enumerate() {
        // swarm: a switched-off kind = a whole poll (learning + whole polls always on)
        let kind = match &step {
            Step::Mine { .. } => Some(0),
            Step::Relay { .. } => Some(1),
            Step::Fork { .. } => Some(2),
            Step::Finalize => Some(3),
            Step::Serve { .. } => Some(4),
            Step::Poll { answer: Answer::Partial { .. } | Answer::Failed, .. } => Some(5),
            Step::Poll { answer: Answer::Raced { .. }, .. } => Some(6),
            Step::Learn { .. } | Step::Poll { answer: Answer::Full, .. } => None,
        };
        let step = match kind.is_some_and(|kind| off & (1 << kind) != 0) {
            true => Step::Poll { node: at, answer: Answer::Full },
            false => step,
        };
        match step {
            Step::Mine { node, count } => mine(&mut world, &mut nodes[node % n], count),
            Step::Relay { from, to } => {
                let (from, to) = (from % n, to % n);
                if nodes[from].len() > nodes[to].len() {
                    nodes[to] = nodes[from].clone();
                }
            }
            Step::Fork { node, drop, mine: count } => {
                fork(&mut world, &mut nodes[node % n], drop, count)
            }
            Step::Learn { node } => {
                let path = world.path(tip(&nodes[node % n]).hash);
                let above = ours.final_tip().map_or(0, |tip| u32::from(tip.height) as usize + 1);
                let _refused = ours.insert_blocks(path.get(above..).unwrap_or_default());
                verified = ours.verified();
                holders.verified(verified.clone().map(Arc::new));
            }
            Step::Finalize => {
                if let Some(boundary) = ours.finalizable() {
                    ours.finalize(boundary).expect("SimFs commits");
                    verified = ours.verified();
                    holders.verified(verified.clone().map(Arc::new));
                }
            }
            Step::Poll { node, answer } => {
                let (node, asked) = (node % n, holders.asked());
                let endpoint = EndpointIndex::new(node).expect("< MAX");
                let under = verified.as_ref().map(VerifiedChain::best);
                let answered = |path: &[BlockHash]| -> Vec<BlockRef> {
                    let at = |height: &Height| Some(*path.get(u32::from(*height) as usize)?);
                    asked
                        .iter()
                        .filter_map(|h| Some(BlockRef { hash: at(h)?, height: *h }))
                        .collect()
                };
                match answer {
                    Answer::Failed => {
                        holders.lost(endpoint);
                        known[node] = Known::Silent;
                    }
                    Answer::Full | Answer::Partial { .. } => {
                        let path = nodes[node].clone();
                        let mut held = answered(&path);
                        let mut whole = true;
                        if let (Answer::Partial { dropped }, false) = (answer, held.is_empty()) {
                            held.remove(dropped % held.len());
                            whole = false;
                        }
                        holders.polled(endpoint, tip(&path), held);
                        let moments = vec![path.clone()];
                        known[node] = Known::Polled { claim: tip(&path), moments, under, whole };
                    }
                    Answer::Raced { drop, mine: count } => {
                        let before = nodes[node].clone();
                        fork(&mut world, &mut nodes[node], drop, count);
                        let after = nodes[node].clone();
                        holders.polled(endpoint, tip(&before), answered(&after));
                        let moments = vec![before.clone(), after];
                        let claim = tip(&before);
                        known[node] = Known::Polled { claim, moments, under, whole: false };
                    }
                }
            }
            Step::Serve { node, below } => {
                let node = node % n;
                let path = &nodes[node];
                let height = (path.len() - 1).saturating_sub(below as usize);
                let block = BlockRef {
                    hash: path[height],
                    height: Height::try_from(height as u32).expect("small"),
                };
                let endpoint = EndpointIndex::new(node).expect("< MAX");
                holders.served(endpoint, block);
                if let Known::Polled { moments, whole, .. } = &mut known[node] {
                    moments.push(path.clone());
                    *whole = false;
                }
            }
        }

        let context = format!("step {at}: {known:?}");
        holders.check();
        verify(&holders, verified.as_ref(), &known, depth, &context);
    }
}

/// V1 + V2 for every validator against `verified`, from the oracle's whole chains
fn verify(
    holders: &Holders,
    verified: Option<&VerifiedChain>,
    known: &[Known],
    depth: ReorgDepth,
    context: &str,
) {
    let Some(verified) = verified else {
        assert!(holders.asked().is_empty(), "{context}: nothing verified, nothing asked");
        for at in (0..known.len()).filter_map(EndpointIndex::new) {
            assert_eq!(holders.agreement(at), Agreement::Unknown, "{context}: {at:?}");
        }
        return;
    };
    let best = verified.best();
    let block =
        |height: Height| BlockRef { hash: verified.hash_at(height).expect("≤ best"), height };
    let boundary = best.height.checked_sub(depth.get());
    let asked: Vec<Height> = boundary.into_iter().chain([best.height]).collect();
    assert_eq!(holders.asked(), asked, "{context}: the boundary + the best, ascending");

    let heights = || Height::GENESIS.up_to(best.height);
    let sets: Vec<_> = heights().map(|height| holders.holders(block(height))).collect();
    for (at, known) in known.iter().enumerate() {
        let endpoint = EndpointIndex::new(at).expect("< MAX");
        let held = |height: Height| sets[u32::from(height) as usize].contains(endpoint);
        match known {
            Known::Silent => {
                let any = heights().find(|h| held(*h));
                assert_eq!(any, None, "{context}: V1 validator {at} silent or lost holds nothing");
                assert_eq!(holders.agreement(endpoint), Agreement::Unknown, "{context}: V2 {at}");
                assert_eq!(holders.claim(endpoint), None, "{context}: {at}");
            }
            Known::Polled { claim, moments, under, whole } => {
                for height in heights() {
                    let truth = moments.iter().any(|path| holds(path, block(height)));
                    assert!(
                        !held(height) || truth,
                        "{context}: V1 validator {at} holds {height:?} it never answered holding"
                    );
                }
                let fresh = *whole && *under == Some(best);
                let poll = &moments[0];
                if fresh {
                    for height in &asked {
                        let truth = holds(poll, block(*height));
                        assert_eq!(held(*height), truth, "{context}: V1 fresh {at} at {height:?}");
                    }
                }
                let on_chain = verified.hash_at(claim.height) == Some(claim.hash);
                if on_chain {
                    let missing = Height::GENESIS.up_to(claim.height).find(|h| !held(*h));
                    assert_eq!(missing, None, "{context}: V1 {at}'s verified claim holds below");
                }

                let agreement = holders.agreement(endpoint);
                let naive = if *claim == best {
                    Agreement::Agreed
                } else if holds(poll, best) && claim.height > best.height {
                    Agreement::Ahead
                } else if claim.height < best.height && on_chain {
                    Agreement::Behind
                } else {
                    Agreement::Diverged
                };
                match (fresh, naive) {
                    (true, _) | (false, Agreement::Agreed | Agreement::Behind) => {
                        assert_eq!(agreement, naive, "{context}: V2 validator {at}")
                    }
                    (false, _) => {
                        let ahead = agreement != Agreement::Ahead
                            || (claim.height > best.height
                                && moments.iter().any(|path| holds(path, best)));
                        assert!(ahead, "{context}: V2 validator {at} Ahead unproven");
                    }
                }
            }
        }
    }
}
