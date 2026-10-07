//! Header views on a [`MockChain`]: its header chain and verified chains, under the rules its
//! schedule implies (`Params::regtest` at its Blossom / NU7 heights, any nBits only if varied)

use std::num::NonZeroU32;
use std::path::Path;
use std::sync::Arc;

use zaino_persistence::fs::SimFs;
use zaino_primitives::testing::{encode_header, MockChain, Work};
use zaino_primitives::types::{Block, BlockRef, Height, ReorgDepth};
use zcash_protocol::consensus::NetworkUpgrade;

use crate::{check, decode_header, HeaderChain, HeaderStore, Params, Rejected, VerifiedChain};

pub trait HeaderViews {
    /// Fresh `SimFs` store, genesis inserted
    fn header_chain(&self, depth: ReorgDepth) -> HeaderChain;
    /// Genesis ..= `tip`, nothing final
    fn verified(&self, tip: BlockRef) -> VerifiedChain;
    /// Genesis ..= `tip`, final through `final_at` (< `tip`)
    fn verified_final(&self, tip: BlockRef, final_at: Height) -> VerifiedChain;
}

impl HeaderViews for MockChain {
    fn header_chain(&self, depth: ReorgDepth) -> HeaderChain {
        let schedule = self.schedule();
        let never = Height::try_from(u32::MAX >> 1).expect("the protocol's maximum height");
        let blossom = schedule.upgrades.activation(NetworkUpgrade::Blossom).unwrap_or(never);
        let nu7 = schedule.upgrades.activation(NetworkUpgrade::Nu7);
        let regtest = Params::regtest(blossom, nu7).with_genesis(self.genesis().hash);
        let params = Params { network: schedule.network, ..regtest };
        let params = match schedule.work {
            Work::Limit => params,
            Work::Varied => params.any_bits(),
        };
        let store = HeaderStore::open(SimFs::new(), Path::new("/headers"), schedule.network);
        let mut chain = HeaderChain::open(params, depth, store.expect("a fresh SimFs store opens"));
        insert(&mut chain, &self.blocks(self.genesis())).expect("MockChain genesis verifies");
        chain
    }

    fn verified(&self, tip: BlockRef) -> VerifiedChain {
        let mut chain = self.header_chain(ReorgDepth::CONSENSUS);
        insert(&mut chain, &self.blocks(tip)).expect("every MockChain path verifies (M8)");
        chain.verified().expect("genesis verified")
    }

    fn verified_final(&self, tip: BlockRef, final_at: Height) -> VerifiedChain {
        let deep = u32::from(tip.height).checked_sub(u32::from(final_at)).and_then(NonZeroU32::new);
        let mut chain = self.header_chain(ReorgDepth::new(deep.expect("final_at below tip")));
        insert(&mut chain, &self.blocks(tip)).expect("every MockChain path verifies (M8)");
        let boundary = chain.finalizable().expect("tip = final_at + depth");
        chain.finalize(boundary).expect("SimFs commits");
        chain.verified().expect("genesis verified")
    }
}

/// Each header of `blocks` in order, stage A then B; the first refusal ends it
pub fn insert(chain: &mut HeaderChain, blocks: &[Arc<Block>]) -> Result<(), Rejected> {
    for block in blocks {
        let raw = encode_header(block.header());
        let header = decode_header(&raw).expect("MockChain headers decode");
        chain.insert(&check(chain.params(), header)?, i64::MAX / 2)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use proptest::prelude::*;
    use proptest::sample::Index;
    use zaino_primitives::testing::h;
    use zaino_primitives::types::BlockHash;

    use super::*;
    use crate::Inserted;

    /// Varied chain: views verify it as built, final where asked; the header chain follows its
    /// outweigh; a Limit chain's rules refuse the heavier header (varied work = opt-in)
    #[test]
    fn a_mock_chain_verifies_as_built_and_its_header_chain_follows_the_work() {
        let mut chain = MockChain::regtest().varied_work();
        let twelve = chain.mine_empty(12);
        let verified = chain.verified(twelve);
        assert_eq!((verified.best(), verified.final_tip()), (twelve, None));
        let pinned = chain.verified_final(twelve, h(9));
        assert_eq!((pinned.best(), pinned.final_tip()), (twelve, Some(chain.at(h(9)))));
        assert_eq!(pinned.hash_at(h(5)), Some(chain.at(h(5)).hash));

        let retreat = chain.fork(h(10)).outweigh().mine_empty(1).tip();
        let mut headers = chain.header_chain(ReorgDepth::CONSENSUS);
        insert(&mut headers, &chain.blocks(twelve)).expect("trunk verifies");
        let heavier = decode_header(&chain.header_bytes(retreat.hash)).expect("decodes");
        let bits = heavier.bits();
        let checked = check(headers.params(), heavier).expect("stage A");
        assert_eq!(headers.insert(&checked, i64::MAX / 2), Ok(Inserted::Best { reorg: true }));
        assert_eq!(headers.best().map(|best| best.block), Some(retreat), "12 → 11 by work");
        headers.check();

        let mut strict = MockChain::regtest().header_chain(ReorgDepth::CONSENSUS);
        let refused = insert(&mut strict, &chain.blocks(retreat));
        assert_eq!(refused, Err(Rejected::Difficulty { bits, expected: 0x200f_0f0f }));
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

        /// M5 + M8, each the other's oracle: after every shape (extend, fork, outweigh, revive,
        /// side branch; Limit turns each outweigh into a longer fork) every new block inserts into
        /// the real header chain and `MockChain::tip` = `HeaderChain::best`; outweigh = the best
        #[test]
        fn the_builders_best_tip_is_the_real_header_chains_best(
            varied: bool,
            shapes in proptest::collection::vec(shape(), 1..24),
        ) {
            let mut chain = match varied {
                true => MockChain::regtest().varied_work(),
                false => MockChain::regtest(),
            };
            let mut headers = chain.header_chain(ReorgDepth::CONSENSUS);
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
