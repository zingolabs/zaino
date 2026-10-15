//! Header views on a [`MockChain`]: its header chain (anchored at genesis) and verified chains

use std::num::NonZeroU32;
use std::sync::Arc;

use zaino_primitives::types::{Block, BlockRef, FinalityDepth};
use zaino_source::testing::{encode_header, MockChain, Work};
use zcash_protocol::consensus::{BlockHeight, H0};

use crate::{decode_header, HeaderChain, Params, Rejected, VerifiedChain};

pub trait HeaderViews {
    /// Rules `validate` runs this chain's headers under (Blossom / NU7 at its schedule's heights)
    fn header_params(&self) -> Params;
    /// Anchored at genesis (its finalized tip), nothing above
    fn header_chain(&self, depth: FinalityDepth) -> HeaderChain;
    /// Genesis ..= `tip`, finalized at genesis
    fn verified(&self, tip: BlockRef) -> VerifiedChain;
    /// Genesis ..= `tip`, finalized through `finalized_at` (< `tip`)
    fn verified_finalized(&self, tip: BlockRef, finalized_at: BlockHeight) -> VerifiedChain;
}

impl HeaderViews for MockChain {
    fn header_params(&self) -> Params {
        let schedule = self.schedule();
        let params = Params::regtest(schedule.upgrades).with_genesis(self.genesis().hash);
        match schedule.work {
            Work::Limit => params,
            Work::Varied => params.any_bits(),
        }
    }

    fn header_chain(&self, depth: FinalityDepth) -> HeaderChain {
        let mut chain = HeaderChain::new(depth);
        let genesis = decode_header(&self.header_bytes(self.genesis().hash));
        chain.anchor(&genesis.expect("MockChain headers decode"), H0);
        chain
    }

    fn verified(&self, tip: BlockRef) -> VerifiedChain {
        let mut chain = self.header_chain(FinalityDepth::CONSENSUS);
        insert(&mut chain, &self.blocks(tip)).expect("every MockChain path links (M8)");
        chain.verified().expect("anchored")
    }

    fn verified_finalized(&self, tip: BlockRef, finalized_at: BlockHeight) -> VerifiedChain {
        let deep =
            u32::from(tip.height).checked_sub(u32::from(finalized_at)).and_then(NonZeroU32::new);
        let mut chain =
            self.header_chain(FinalityDepth::new(deep.expect("finalized_at below tip")));
        insert(&mut chain, &self.blocks(tip)).expect("every MockChain path links (M8)");
        chain.finalize(chain.finalizable().expect("tip = finalized_at + depth"));
        chain.verified().expect("anchored")
    }
}

/// Each header of `blocks` in order as a trusted validator's run (the anchor itself = `Known`)
///
/// - last one vouched
/// - first refusal ends it
pub fn insert(chain: &mut HeaderChain, blocks: &[Arc<Block>]) -> Result<(), Rejected> {
    for block in blocks {
        let raw = encode_header(block.header());
        chain.insert(&decode_header(&raw).expect("MockChain headers decode"))?;
    }
    if let Some(last) = blocks.last() {
        chain.vouch(BlockRef { hash: last.header().hash, height: last.header().height });
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
    use crate::Inserted;

    /// - varied chain: views follow it as built, finalized where asked
    /// - header chain follows its outweigh (work decides, not height)
    #[test]
    fn a_mock_chain_verifies_as_built_and_its_header_chain_follows_the_work() {
        let mut chain = MockChain::regtest().varied_work();
        let twelve = chain.mine_empty(12);
        let verified = chain.verified(twelve);
        assert_eq!((verified.best(), verified.finalized_tip()), (twelve, chain.genesis()));
        let pinned = chain.verified_finalized(twelve, h(9));
        assert_eq!((pinned.best(), pinned.finalized_tip()), (twelve, chain.at(h(9))));
        assert_eq!(pinned.hash_at(h(5)), Some(chain.at(h(5)).hash));

        let retreat = chain.fork(h(10)).outweigh().mine_empty(1).tip();
        let mut headers = chain.header_chain(FinalityDepth::CONSENSUS);
        insert(&mut headers, &chain.blocks(twelve)).expect("trunk links");
        let heavier = decode_header(&chain.header_bytes(retreat.hash)).expect("decodes");
        assert_eq!(headers.insert(&heavier), Ok(Inserted::Best { reorg: true }));
        assert_eq!(headers.best().map(|best| best.block), Some(retreat), "12 → 11 by work");
        headers.check();
    }

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

        /// M5 + M8, each the other's oracle, after every shape (extend, fork, outweigh, revive,
        /// side branch)
        ///
        /// - every new block inserts into the real header chain, `MockChain::tip` =
        ///   `HeaderChain::best`
        /// - outweigh = the best (Limit turns each outweigh into a longer fork)
        #[test]
        fn the_builders_best_tip_is_the_real_header_chains_best(
            varied: bool,
            shapes in proptest::collection::vec(shape(), 1..24),
        ) {
            let mut chain = match varied {
                true => MockChain::regtest().varied_work(),
                false => MockChain::regtest(),
            };
            let mut headers = chain.header_chain(FinalityDepth::CONSENSUS);
            let mut inserted: HashSet<BlockHash> = HashSet::from([chain.genesis().hash]);
            let mut tips = vec![chain.genesis()];
            for shape in shapes {
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
                    prop_assert_eq!(chain.tip(), mined, "outweigh / a longer fork = the best");
                }
                tips.push(mined);
                let fresh: Vec<Arc<Block>> = chain
                    .blocks(mined)
                    .into_iter()
                    .filter(|block| inserted.insert(block.header().hash))
                    .collect();
                prop_assert_eq!(insert(&mut headers, &fresh), Ok(()));
                headers.check();
                prop_assert_eq!(headers.best().map(|best| best.block), Some(chain.tip()));
            }
        }
    }
}
