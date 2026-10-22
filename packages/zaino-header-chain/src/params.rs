//! Per-network consensus constants the header rules read (zebra-chain `src/parameters/`)

use zaino_primitives::protocol::compiled_schedule;
use zaino_primitives::types::{CompactDifficulty, Target};
use zcash_protocol::consensus::{
    target_spacing, BlockHeight, NetworkType, NetworkUpgrade, Parameters,
};
use zcash_protocol::local_consensus::LocalNetwork;
use zcash_protocol::BlockHash;

/// One network's header rules ([`mainnet`](Self::mainnet), [`testnet`](Self::testnet),
/// [`regtest`](Self::regtest))
///
/// - `pow_limit` = `PoWLimit` compact-rounded (zcashd compares that)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Params {
    pub(crate) genesis: BlockHash,
    pub(crate) schedule: LocalNetwork,
    pub(crate) pow_limit: Target,
    pub(crate) pow: bool,
    pub(crate) difficulty: Difficulty,
    pub(crate) min_difficulty_from: Option<BlockHeight>,
    pub(crate) max_time_from: Option<BlockHeight>,
}

/// Source of a header's nBits: the adjustment, the limit (zebra regtest), any (model tests)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Difficulty {
    Adjusted,
    Limit,
    #[cfg(test)]
    Any,
}

pub(crate) const AVERAGING_WINDOW: usize = 17;
/// ZIP 218 `PostNU7PoWAveragingWindow`
const POST_NU7_AVERAGING_WINDOW: usize = 102;
pub(crate) const MAX_AVERAGING_WINDOW: usize = POST_NU7_AVERAGING_WINDOW;

/// Testnet: a block more than this many spacings after its parent may use the minimum difficulty
const MIN_DIFFICULTY_GAP_MULTIPLIER: i64 = 6;
/// From NU7: 18 × 25 s keeps the 450 s gap (ZIP 218 "Minimum difficulty blocks on Testnet")
const POST_NU7_MIN_DIFFICULTY_GAP_MULTIPLIER: i64 = 18;

impl Params {
    /// `network`'s rules (regtest's heights from `schedule`)
    pub fn for_network(network: NetworkType, schedule: &LocalNetwork) -> Self {
        match network {
            NetworkType::Main => Self::mainnet(),
            NetworkType::Test => Self::testnet(),
            NetworkType::Regtest => Self::regtest(*schedule),
        }
    }

    pub fn mainnet() -> Self {
        Self {
            genesis: display("00040fe8ec8471911baa1db1266ea15dd06b4a8a5c453883c000b031973dce08"),
            schedule: compiled(NetworkType::Main),
            pow_limit: limit("0007ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"),
            pow: true,
            difficulty: Difficulty::Adjusted,
            min_difficulty_from: None,
            // - protocol spec §7.6 "block height 2 or greater" (block 1 = genesis + 8.4 h: fails)
            // - zebra `is_max_block_time_enforced`: every height (never reached under checkpoints)
            max_time_from: Some(height(2)),
        }
    }

    /// Default testnet (`network_upgrade.rs`: min difficulty from 299,188, max time from 653,606)
    pub fn testnet() -> Self {
        Self {
            genesis: display("05a60a92d99d85997cce3b87616c089f6124d7342af37106edc76126334a2c38"),
            schedule: compiled(NetworkType::Test),
            pow_limit: limit("07ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"),
            pow: true,
            difficulty: Difficulty::Adjusted,
            min_difficulty_from: Some(height(299_188)),
            max_time_from: Some(height(653_606)),
        }
    }

    /// Proof of work off (Equihash, hash ≤ target), nBits fixed at the limit
    ///
    /// - linkage, time and work still enforced (`testnet.rs` `new_regtest`: a configured testnet,
    ///   so testnet's max-time height applies too)
    /// - `schedule` only moves the averaging window at NU7 (nBits = the limit at every height)
    pub fn regtest(schedule: LocalNetwork) -> Self {
        Self {
            genesis: display("029f11d80ef9765602235e1bc9727e3eb6ba20839319f761fee920d63401e327"),
            schedule,
            pow_limit: limit("0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f"),
            pow: false,
            difficulty: Difficulty::Limit,
            min_difficulty_from: None,
            max_time_from: Some(height(653_606)),
        }
    }

    /// Same rules over another genesis (tests building their own chains)
    #[cfg(test)]
    pub(crate) fn with_genesis(self, genesis: BlockHash) -> Self {
        Self { genesis, ..self }
    }

    /// Any valid nBits accepted (model tests: work varies per branch)
    #[cfg(test)]
    pub(crate) fn any_bits(self) -> Self {
        Self { difficulty: Difficulty::Any, ..self }
    }

    fn nu7_active(&self, at: BlockHeight) -> bool {
        self.schedule.is_nu_active(NetworkUpgrade::Nu7, at)
    }

    /// `PoWTargetSpacing(height)`, seconds
    pub(crate) fn spacing(&self, at: BlockHeight) -> i64 {
        i64::from(target_spacing(&self.schedule, at))
    }

    /// `PoWAveragingWindow(height)` (ZIP 218: every use in §7.7.3 takes the checked height's)
    pub(crate) fn averaging_window(&self, at: BlockHeight) -> usize {
        match self.nu7_active(at) {
            true => POST_NU7_AVERAGING_WINDOW,
            false => AVERAGING_WINDOW,
        }
    }

    /// Gap after which a testnet block may drop to the minimum difficulty (`None` = never)
    pub(crate) fn min_difficulty_gap(&self, at: BlockHeight) -> Option<i64> {
        let from = self.min_difficulty_from?;
        let multiplier = match self.nu7_active(at) {
            true => POST_NU7_MIN_DIFFICULTY_GAP_MULTIPLIER,
            false => MIN_DIFFICULTY_GAP_MULTIPLIER,
        };
        (at >= from).then(|| self.spacing(at) * multiplier)
    }

    pub(crate) fn max_time_enforced(&self, at: BlockHeight) -> bool {
        self.max_time_from.is_some_and(|from| at >= from)
    }

    /// Limit as compact nBits (regtest's every block, testnet's minimum)
    pub(crate) fn limit_bits(&self) -> u32 {
        self.pow_limit.to_compact()
    }
}

/// zcashd `chainparams.cpp` `powLimit` (display-order hex), compact-rounded
fn limit(pow_limit: &str) -> Target {
    let exact = Target::from_hex(pow_limit).expect("powLimit: constant display-order hex");
    let rounded = CompactDifficulty::try_from_bits(exact.to_compact());
    rounded.expect("a network's limit is a valid target").target()
}

/// `zcash_protocol`'s tables (= the pool activations')
fn compiled(network: NetworkType) -> LocalNetwork {
    compiled_schedule(network).expect("mainnet + testnet compiled in")
}

fn height(height: u32) -> BlockHeight {
    BlockHeight::from(height)
}

fn display(hex: &str) -> BlockHash {
    BlockHash::from_hex(hex).expect("genesis: constant display-order hex")
}

#[cfg(test)]
mod tests {
    use zaino_source::testing::{all_at, h};

    use super::*;

    /// `powLimit` → nBits as zcashd's chainparams encode them
    #[test]
    fn each_networks_limit_nbits_is_zcashds() {
        let regtest = Params::regtest(all_at(h(1)));
        #[rustfmt::skip]
        let cases = [
            ("mainnet", Params::mainnet(), 0x1f07_ffff),
            ("testnet", Params::testnet(), 0x2007_ffff),
            ("regtest", regtest,           0x200f_0f0f),
        ];
        for (network, params, bits) in cases {
            let limit = params.limit_bits();
            assert_eq!(limit, bits, "{network}: {limit:#010x} vs zcashd {bits:#010x}");
        }
    }

    /// Spacing, averaging window and testnet's minimum-difficulty gap switch at Blossom and NU7
    #[test]
    fn spacing_window_and_minimum_difficulty_gap_switch_at_blossom_and_nu7() {
        #[derive(Debug, PartialEq)]
        struct Rules {
            spacing: i64,
            window: usize,
            min_difficulty_gap: Option<i64>,
        }
        let rules = |params: Params, at: u32| Rules {
            spacing: params.spacing(h(at)),
            window: params.averaging_window(h(at)),
            min_difficulty_gap: params.min_difficulty_gap(h(at)),
        };
        let expect =
            |spacing, window, min_difficulty_gap| Rules { spacing, window, min_difficulty_gap };
        let regtest_nu7 = Params::regtest(LocalNetwork { nu7: Some(h(10)), ..all_at(h(1)) });
        #[rustfmt::skip]
        let cases = [
            // (network, params,           height,    expect(spacing s, window, min-difficulty gap s))
            ("mainnet", Params::mainnet(), 653_599,   expect(150, 17,  None)),
            ("mainnet", Params::mainnet(), 653_600,   expect(75,  17,  None)),
            ("mainnet", Params::mainnet(), 9_999_999, expect(75,  17,  None)),
            ("testnet", Params::testnet(), 299_187,   expect(150, 17,  None)),
            ("testnet", Params::testnet(), 299_188,   expect(150, 17,  Some(900))),
            ("testnet", Params::testnet(), 584_000,   expect(75,  17,  Some(450))),
            ("testnet", Params::testnet(), 4_465_025, expect(75,  17,  Some(450))),
            ("testnet", Params::testnet(), 4_465_026, expect(25,  102, Some(450))),
            ("regtest", regtest_nu7,       10,        expect(25,  102, None)),
        ];
        for (network, params, height, expected) in cases {
            assert_eq!(rules(params, height), expected, "{network} at {height}");
        }
    }
}
