use std::sync::Arc;

use proptest::{
    prelude::{Arbitrary as _, BoxedStrategy, Just},
    strategy::Strategy,
};
use zebra_chain::{
    block::arbitrary::{self, LedgerStateOverride},
    fmt::SummaryDebug,
    parameters::GENESIS_PREVIOUS_BLOCK_HASH,
    LedgerState,
};

#[test]
fn make_chain() {
    proptest::proptest!(|(segments in make_branching_chain(2, 12))| {
        let (genesis_segment, branch_segments) = segments;
        let mut prev_hash = GENESIS_PREVIOUS_BLOCK_HASH;
        for block in genesis_segment {
            assert_eq!(block.header.previous_block_hash, prev_hash);
            println!("pre-divergence: {:?}", block.coinbase_height());
            prev_hash = block.hash();

        }
        let hash_atop_shared_chain = prev_hash;
        for branch_segment in branch_segments {
            for block in branch_segment {
            assert_eq!(block.header.previous_block_hash, prev_hash);
                println!("post-divergence: {:?}", block.coinbase_height());
            prev_hash = block.hash();
            }
            prev_hash = hash_atop_shared_chain;
        }
    });
}

fn make_branching_chain(
    num_branches: usize,
    chain_size: usize,
) -> BoxedStrategy<(
    SummaryDebug<Vec<Arc<zebra_chain::block::Block>>>,
    Vec<SummaryDebug<Vec<Arc<zebra_chain::block::Block>>>>,
)> {
    arbitrary::LedgerState::genesis_strategy(None, None, true)
        .prop_flat_map(move |ledger| {
            zebra_chain::block::Block::partial_chain_strategy(
                ledger,
                chain_size,
                arbitrary::allow_all_transparent_coinbase_spends,
                false,
            )
        })
        .prop_flat_map(|segment| {
            (
                Just(segment.clone()),
                LedgerState::arbitrary_with(LedgerStateOverride {
                    height_override: segment.last().unwrap().coinbase_height().unwrap() + 1,
                    previous_block_hash_override: Some(segment.last().unwrap().hash()),
                    network_upgrade_override: None,
                    transaction_version_override: None,
                    transaction_has_valid_network_upgrade: true,
                    always_has_coinbase: true,
                }),
            )
        })
        .prop_flat_map(move |(segment, ledger)| {
            (
                Just(segment),
                std::iter::repeat_with(|| {
                    zebra_chain::block::Block::partial_chain_strategy(
                        ledger.clone(),
                        chain_size,
                        arbitrary::allow_all_transparent_coinbase_spends,
                        false,
                    )
                })
                .take(num_branches)
                .collect::<Vec<_>>(),
            )
        })
        .boxed()
}
