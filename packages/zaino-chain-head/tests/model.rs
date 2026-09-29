//! `ChainHead` against a model of the best chain: every advance classified exactly (`Unchanged`,
//! `Extended`, `Reorg { fork }` at the first differing height, `BelowWindow` iff the fork parent
//! sits under the floor), window = the model's chain from the model's floor
//!
//! - Two validators, one serving the previous move's chain: spread height fetches straddle
//!   branches, hash lookups fall past a node that lacks the block

use std::collections::HashMap;
use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::Arc;

use proptest::prelude::*;
use zaino_chain_head::{Advance, AdvanceError, ChainHead};
use zaino_primitives::types::{
    Block, BlockHash, BlockHeader, BlockRef, Height, ReorgDepth, Transaction,
};
use zaino_source::{mock::MockChain, BlockFetchPool, FetchRoute};

const DEPTH: u32 = 4;
/// Head anchored here, then advanced onto `ANCHOR + DEPTH` (a full window before the first move)
const ANCHOR: u32 = 6;

#[derive(Debug, Clone)]
enum Move {
    Same,
    Extend(u32),
    /// Top `depth` blocks replaced by `len` (0 = retreat; past the floor = refused)
    Reorg {
        depth: u32,
        len: u32,
    },
}

fn moves() -> impl Strategy<Value = Vec<Move>> {
    prop::collection::vec(
        prop_oneof![
            1 => Just(Move::Same),
            3 => (1u32..=DEPTH).prop_map(Move::Extend),
            5 => (1u32..=DEPTH + 2).prop_flat_map(|depth| {
                (Just(depth), 0u32..=depth + DEPTH).prop_map(|(depth, len)| Move::Reorg { depth, len })
            }),
        ],
        1..32,
    )
}

/// Hash = (height, branch): every branch's block at a height is distinct
fn hash(height: u32, branch: u16) -> BlockHash {
    let mut bytes = [0u8; 32];
    bytes[..4].copy_from_slice(&height.to_le_bytes());
    bytes[4..6].copy_from_slice(&branch.to_le_bytes());
    BlockHash::from(bytes)
}

fn block(height: u32, own: BlockHash, parent: BlockHash) -> Block {
    Block::new(
        BlockHeader::for_tests(height, own.into(), parent.into(), 0),
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

fn height(h: usize) -> Height {
    Height::try_from(h as u32).expect("model height")
}

proptest! {
    #[test]
    fn advance_matches_a_model_of_the_best_chain(
        moves in moves(),
        lagging_first in any::<bool>(),
        concurrency in 1usize..=4,
    ) {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime")
            .block_on(run(moves, lagging_first, concurrency));
    }
}

async fn run(moves: Vec<Move>, lagging_first: bool, concurrency: usize) {
    let mut blocks: HashMap<BlockHash, Block> = HashMap::new();
    let mut best: Vec<BlockHash> = Vec::new();
    for h in 0..=ANCHOR + DEPTH {
        let parent = best.last().copied().unwrap_or(hash(u32::MAX, 0));
        let own = hash(h, 0);
        blocks.insert(own, block(h, own, parent));
        best.push(own);
    }
    let (current, lagging) = (Arc::new(MockChain::new()), Arc::new(MockChain::new()));
    let serve = |mock: &MockChain, chain: &[BlockHash], blocks: &HashMap<BlockHash, Block>| {
        mock.rewind_to(height(0));
        mock.extend_best(chain.iter().map(|own| blocks[own].clone()));
    };
    serve(&current, &best, &blocks);
    serve(&lagging, &best, &blocks);
    let sources = match lagging_first {
        true => vec![Arc::clone(&lagging), Arc::clone(&current)],
        false => vec![Arc::clone(&current), Arc::clone(&lagging)],
    };
    let concurrency = NonZeroUsize::new(concurrency).expect("1..=4");
    let pool = BlockFetchPool::new(sources, FetchRoute::Spread, concurrency);

    let anchor = Arc::new(blocks[&best[ANCHOR as usize]].clone());
    let depth = ReorgDepth::new(NonZeroU32::new(DEPTH).expect("depth"));
    let mut head = ChainHead::new(anchor, depth);
    let tip_of = |chain: &[BlockHash]| BlockRef {
        hash: *chain.last().expect("genesis kept"),
        height: height(chain.len() - 1),
    };
    let window = |head: &ChainHead, from: usize| -> Vec<BlockHash> {
        head.best_chain_from(height(from)).map(|block| block.header().hash).collect()
    };
    let filled = head.advance(tip_of(&best), &pool).await.expect("fill the window");
    assert_eq!(filled, Advance::Extended, "anchor → pre-mined tip");
    let (mut floor, mut highest) = (ANCHOR as usize, best.len() - 1);

    for (step, change) in moves.iter().enumerate() {
        let old = best.clone();
        let tip = old.len() - 1;
        let (keep, grow) = match *change {
            Move::Same => (old.len(), 0),
            Move::Extend(count) => (old.len(), count as usize),
            Move::Reorg { depth, len } => {
                let depth = (depth as usize).min(tip);
                (old.len() - depth, (len as usize).min(depth + DEPTH as usize))
            }
        };
        best.truncate(keep);
        for _ in 0..grow {
            let (h, parent) = (best.len() as u32, *best.last().expect("genesis kept"));
            let own = hash(h, step as u16 + 1);
            blocks.insert(own, block(h, own, parent));
            best.push(own);
        }
        serve(&lagging, &old, &blocks);
        serve(&current, &best, &blocks);

        // trim at the start of the advance: highest before it − depth, tip always kept
        floor = floor.max(highest.saturating_sub(DEPTH as usize)).min(tip);
        let first_diff = old.iter().zip(&best).position(|(was, now)| was != now);
        let expected = match (first_diff, best.len().cmp(&old.len())) {
            (None, std::cmp::Ordering::Equal) => Advance::Unchanged,
            (None, std::cmp::Ordering::Greater) => Advance::Extended,
            (None, std::cmp::Ordering::Less) => Advance::Reorg { fork: height(best.len()) },
            (Some(fork), _) => Advance::Reorg { fork: height(fork) },
        };
        let legal = match expected {
            Advance::Reorg { fork } => u32::from(fork) as usize > floor,
            Advance::Unchanged | Advance::Extended => true,
        };
        let context = format!("move {step} {change:?}: {} → {} blocks", old.len(), best.len());

        let outcome = head.advance(tip_of(&best), &pool).await;
        assert_eq!(u32::from(head.floor()) as usize, floor, "{context}: floor");
        if !legal {
            let Err(AdvanceError::BelowWindow { floor: refused, .. }) = &outcome else {
                panic!("{context}: fork under floor {floor}, got {outcome:?}")
            };
            assert_eq!(u32::from(*refused) as usize, floor, "{context}: refused floor");
            assert_eq!(head.tip(), tip_of(&old), "{context}: refused advance kept the tip");
            assert_eq!(window(&head, floor), old[floor..], "{context}: refused window untouched");
            best = old;
            serve(&current, &best, &blocks);
            continue;
        }
        let outcome = outcome.unwrap_or_else(|error| panic!("{context}: {error}"));
        assert_eq!(outcome, expected, "{context}");
        assert_eq!(head.tip(), tip_of(&best), "{context}: tip");
        assert_eq!(window(&head, floor), best[floor..], "{context}: window from floor {floor}");
        highest = highest.max(best.len() - 1);
    }
}
