//! Header chains of a [`MockChain`]: anchored at its genesis, snapshotted as built

use std::num::NonZeroU32;
use std::sync::Arc;

use zaino_primitives::types::{Block, BlockRef, FinalityDepth};
use zaino_source::testing::{encode_header, MockChain};
use zcash_protocol::consensus::{BlockHeight, H0};

use crate::{decode_header, HeaderChain, HeaderChainSnapshot, Rejected, Witness};

impl HeaderChain {
    /// `chain`'s genesis = its finalized tip, nothing above
    pub fn anchored(chain: &MockChain, depth: FinalityDepth) -> Self {
        let mut headers = Self::new(depth);
        let genesis = decode_header(&chain.header_bytes(chain.genesis().hash));
        headers.anchor(&genesis.expect("MockChain headers decode"), H0);
        headers
    }
}

impl HeaderChainSnapshot {
    /// `chain`'s genesis ..= `tip`, finalized at genesis
    pub fn of(chain: &MockChain, tip: BlockRef) -> Self {
        let mut headers = HeaderChain::anchored(chain, FinalityDepth::CONSENSUS);
        insert(&mut headers, &chain.blocks(tip)).expect("every MockChain path links (M8)");
        headers.snapshot().expect("anchored")
    }

    /// `chain`'s genesis ..= `tip`, finalized through `finalized_at` (< `tip`)
    pub fn finalized(chain: &MockChain, tip: BlockRef, finalized_at: BlockHeight) -> Self {
        let deep =
            u32::from(tip.height).checked_sub(u32::from(finalized_at)).and_then(NonZeroU32::new);
        let depth = FinalityDepth::new(deep.expect("finalized_at below tip"));
        let mut headers = HeaderChain::anchored(chain, depth);
        insert(&mut headers, &chain.blocks(tip)).expect("every MockChain path links (M8)");
        headers.finalize(headers.finalizable().expect("tip = finalized_at + depth"));
        headers.snapshot().expect("anchored")
    }
}

/// Each header of `blocks` in order as a trusted validator's run (the anchor itself = `Known`)
///
/// - last one vouched
/// - first refusal ends it
pub fn insert(chain: &mut HeaderChain, blocks: &[Arc<Block>]) -> Result<(), Rejected> {
    insert_unwitnessed(chain, blocks)?;
    if let Some(last) = blocks.last() {
        chain.witness(last.header().hash, Witness::Trusted);
    }
    Ok(())
}

/// [`insert`], nothing witnessed (peers' headers, no `inv` yet)
pub fn insert_unwitnessed(chain: &mut HeaderChain, blocks: &[Arc<Block>]) -> Result<(), Rejected> {
    for block in blocks {
        let raw = encode_header(block.header());
        chain.insert(&decode_header(&raw).expect("MockChain headers decode"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use proptest::prelude::*;
    use proptest::sample::Index;
    use zaino_source::testing::h;
    use zcash_protocol::BlockHash;

    use super::*;

    #[derive(Debug, Clone)]
    enum Shape {
        Extend(u32),
        Fork { back: u32, len: u32 },
        Outweigh { back: u32 },
        Revive(Index),
        Side { back: u32, len: u32 },
    }

    fn shape() -> impl Strategy<Value = Shape> {
        prop_oneof![
            (1..4u32).prop_map(Shape::Extend),
            (0..6u32, 1..4u32).prop_map(|(back, len)| Shape::Fork { back, len }),
            (0..6u32).prop_map(|back| Shape::Outweigh { back }),
            any::<Index>().prop_map(Shape::Revive),
            (1..6u32, 1..4u32).prop_map(|(back, len)| Shape::Side { back, len }),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

        /// Builder and real header chain, each the other's oracle, after every shape (extend,
        /// fork, outweigh, revive, side branch)
        ///
        /// - every new block inserts, `MockChain::tip` = `HeaderChain::best` (M8)
        /// - outweigh = the best (M5: `Work::Limit` turns each outweigh into a longer fork)
        #[test]
        fn the_builders_best_tip_is_the_real_header_chains_best(
            varied: bool,
            shapes in proptest::collection::vec(shape(), 1..24),
        ) {
            let mut chain = match varied {
                true => MockChain::regtest().varied_work(),
                false => MockChain::regtest(),
            };
            let mut headers = HeaderChain::anchored(&chain, FinalityDepth::CONSENSUS);
            let mut inserted: HashSet<BlockHash> = HashSet::from([chain.genesis().hash]);
            let mut tips = vec![chain.genesis()];
            for (step, shape) in shapes.into_iter().enumerate() {
                let case = format!("step {step} {shape:?}");
                let tip = chain.tip();
                let at = |back: u32| h(u32::from(tip.height).saturating_sub(back));
                let wins = matches!(shape, Shape::Outweigh { .. } | Shape::Revive(_));
                let mined = match shape {
                    Shape::Extend(count) => chain.mine_empty(count),
                    Shape::Fork { back, len } => chain.fork(at(back)).mine_empty(len).tip(),
                    Shape::Outweigh { back } if varied => {
                        chain.fork(at(back)).outweigh().mine_empty(1).tip()
                    }
                    Shape::Outweigh { back } => chain.fork(at(back)).mine_empty(back + 1).tip(),
                    Shape::Revive(pick) => {
                        let old = *pick.get(&tips);
                        let behind = u32::from(tip.height) - u32::from(old.height).min(u32::from(tip.height));
                        match varied {
                            true => chain.branch(old).outweigh().mine_empty(1).tip(),
                            false => chain.branch(old).mine_empty(behind + 1).tip(),
                        }
                    }
                    Shape::Side { back, len } => {
                        chain.fork(at(back)).mine_empty(len.min(back)).tip()
                    }
                };
                if wins {
                    prop_assert_eq!(chain.tip(), mined, "{}: outweigh / a longer fork = the best", case);
                }
                tips.push(mined);
                let fresh: Vec<Arc<Block>> = chain
                    .blocks(mined)
                    .into_iter()
                    .filter(|block| inserted.insert(block.header().hash))
                    .collect();
                prop_assert_eq!(insert(&mut headers, &fresh), Ok(()), "{}: builder's blocks insert", case);
                headers.check();
                let best = headers.best().map(|best| best.block);
                prop_assert_eq!(best, Some(chain.tip()), "{}: header chain best vs builder tip", case);
            }
        }
    }
}
