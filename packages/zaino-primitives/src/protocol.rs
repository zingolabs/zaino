//! Zcash protocol constants and rules (specification facts, restating them elsewhere = bug)

use zcash_protocol::consensus::{BlockHeight, NetworkType, Parameters, MAIN_NETWORK, TEST_NETWORK};
use zcash_protocol::local_consensus::LocalNetwork;
use zcash_protocol::value::Zatoshis;

use crate::types::PerPool;

/// ZIP 317 marginal fee, zatoshis per logical action, before ZIP 2009 lowered it
const LEGACY_MARGINAL_FEE: u64 = 5_000;

/// ZIP 2009's marginal fee
const MARGINAL_FEE: u64 = 1_000;

/// Mainnet height wallets switch to [`MARGINAL_FEE`] at (draft ZIP, zcash/zips#1352: zebrad's
/// `getstandardfee`, after the v6.3.0 end-of-support halt)
const MAINNET_MARGINAL_FEE_HEIGHT: u32 = 3_590_000;

/// Fee per logical action a transaction mined at `height` pays (`GetStandardFee`; test networks =
/// [`MARGINAL_FEE`] at every height)
pub fn standard_fee(network: NetworkType, height: BlockHeight) -> Zatoshis {
    let legacy = network == NetworkType::Main && u32::from(height) < MAINNET_MARGINAL_FEE_HEIGHT;
    Zatoshis::const_from_u64(if legacy { LEGACY_MARGINAL_FEE } else { MARGINAL_FEE })
}

/// Mainnet's or testnet's activation heights, as librustzcash compiles them (`None` = regtest:
/// heights set per chain, read from its validator)
pub fn compiled_schedule(network: NetworkType) -> Option<LocalNetwork> {
    match network {
        NetworkType::Main => Some(LocalNetwork::from_activation_heights(|upgrade| {
            MAIN_NETWORK.activation_height(upgrade)
        })),
        NetworkType::Test => Some(LocalNetwork::from_activation_heights(|upgrade| {
            TEST_NETWORK.activation_height(upgrade)
        })),
        NetworkType::Regtest => None,
    }
}

/// Height each pool's tree begins (`None` = unscheduled)
///
/// - zebra's `z_gettreestate` omits a pool below its upgrade (lightwalletd then answers `""`)
pub fn pool_activations(schedule: &LocalNetwork) -> PerPool<Option<BlockHeight>> {
    PerPool::from_fn(|pool| schedule.activation_height(pool.activation_upgrade()))
}

/// No valid reorg rewrites more than this many blocks
pub const MAX_BLOCK_REORG_HEIGHT: u32 = 1000;

/// Serialized block ceiling (protocol spec §7.1 `MAX_BLOCK_SIZE`, transaction size ≤ it)
pub const MAX_BLOCK_BYTES: usize = 2_000_000;

/// Ancestors whose times a median time past reads (protocol spec `PoWMedianBlockSpan`)
pub const MEDIAN_TIME_SPAN: usize = 11;

/// Upper median of the first [`MEDIAN_TIME_SPAN`] of `newest_first` (`None` = no times)
pub fn median_time_past(newest_first: impl IntoIterator<Item = u32>) -> Option<u32> {
    let mut times: Vec<u32> = newest_first.into_iter().take(MEDIAN_TIME_SPAN).collect();
    times.sort_unstable();
    times.get(times.len() / 2).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// - unsorted input, only the newest 11 count
    /// - even count → upper middle
    #[test]
    fn median_time_past_is_the_upper_median_of_the_newest_eleven() {
        let newest_first = [9, 1, 8, 2, 7, 3, 6, 4, 5, 10, 11, 1_000, 2_000];
        assert_eq!(median_time_past(newest_first), Some(6), "newest 11 of {newest_first:?}");
        assert_eq!(median_time_past([4, 1, 3, 2]), Some(3), "upper middle of [1 2 3 4]");
        assert_eq!(median_time_past([]), None, "no times");
    }

    /// - Sapling, NU5 (orchard), NU6.3 (ironwood) from the schedule
    /// - unscheduled pool = `None`
    #[test]
    fn pool_activations_are_the_schedules_pool_upgrades() {
        use zcash_protocol::consensus::NetworkUpgrade;

        let h = BlockHeight::from;
        let mainnet = compiled_schedule(NetworkType::Main).expect("mainnet compiled in");
        let expected = PerPool {
            sapling: Some(h(419_200)),
            orchard: Some(h(1_687_104)),
            ironwood: Some(h(3_428_143)),
        };
        assert_eq!(pool_activations(&mainnet), expected, "mainnet: Sapling, NU5, NU6.3");

        let no_nu5 = LocalNetwork::from_activation_heights(|upgrade| {
            (!matches!(upgrade, NetworkUpgrade::Nu5 | NetworkUpgrade::Nu7)).then_some(h(1))
        });
        let expected = PerPool { sapling: Some(h(1)), orchard: None, ironwood: Some(h(1)) };
        assert_eq!(pool_activations(&no_nu5), expected, "no NU5 = never orchard");
    }

    #[test]
    fn the_standard_fee_drops_to_a_thousand_at_the_mainnet_switch_and_is_a_thousand_on_test_nets() {
        let at = |network, height: u32| u64::from(standard_fee(network, BlockHeight::from(height)));
        #[rustfmt::skip]
        let cases = [
            (NetworkType::Main,    3_589_999, 5_000),
            (NetworkType::Main,    3_590_000, 1_000),
            (NetworkType::Test,    1,         1_000),
            (NetworkType::Regtest, 1,         1_000),
        ];
        for (network, height, fee) in cases {
            assert_eq!(at(network, height), fee, "{network:?} at {height}");
        }
    }
}
