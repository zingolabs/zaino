use proptest::{strategy::Strategy, test_runner::TestCaseResult};
use zebra_chain::block::arbitrary;

#[test]
fn make_chain() {
    let chain_size = 12;
    let mut runner =
        proptest::test_runner::TestRunner::new(proptest::test_runner::Config::default());
    let overall_strat = arbitrary::LedgerState::genesis_strategy(None, None, true);
    let chain_segment_strat = overall_strat.prop_flat_map(|ledger| {
        zebra_chain::block::Block::partial_chain_strategy(
            ledger,
            chain_size,
            arbitrary::allow_all_transparent_coinbase_spends,
            false,
        )
    });
    runner
        .run(&chain_segment_strat, |segment| {
            for block in segment {
                println!("{:?}", block.coinbase_height())
            }
            Ok(())
        })
        .unwrap();
}
