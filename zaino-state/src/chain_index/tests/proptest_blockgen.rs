use proptest::strategy::Strategy;
use zebra_chain::block::arbitrary;

fn make_chain(chain_size: usize) {
    let overall_strat = arbitrary::LedgerState::default_strategy();
    let chain_segment_strat = overall_strat.prop_map(|ledger| {
        zebra_chain::block::Block::partial_chain_strategy(
            ledger,
            chain_size,
            arbitrary::allow_all_transparent_coinbase_spends,
            false,
        )
    });
}
