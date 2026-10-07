//! Per-network consensus constants the header rules read (zebra-chain v7 `src/parameters/`)

use zaino_primitives::types::{BlockHash, Height};
use zcash_protocol::consensus::NetworkType;

use crate::target::{expand, to_compact, U256};

/// One network's header rules; build with [`mainnet`](Self::mainnet), [`testnet`](Self::testnet)
/// or [`regtest`](Self::regtest)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Params {
    pub(crate) network: NetworkType,
    pub(crate) genesis: BlockHash,
    pub(crate) blossom: Height,
    pub(crate) nu7: Option<Height>,
    /// `PoWLimit`, compact-rounded (zcashd compares against the compact form)
    pub(crate) pow_limit: U256,
    pub(crate) pow: bool,
    pub(crate) difficulty: Difficulty,
    pub(crate) min_difficulty_from: Option<Height>,
    pub(crate) max_time_from: Option<Height>,
}

/// Source of a header's nBits: the adjustment, the limit (zebra regtest), any (model tests)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Difficulty {
    Adjusted,
    Limit,
    #[cfg(any(test, feature = "testing"))]
    Any,
}

const PRE_BLOSSOM_SPACING: i64 = 150;
const POST_BLOSSOM_SPACING: i64 = 75;
/// ZIP 218
const POST_NU7_SPACING: i64 = 25;
const AVERAGING_WINDOW: usize = 17;
/// ZIP 218 `PostNU7PoWAveragingWindow`
const POST_NU7_AVERAGING_WINDOW: usize = 102;
pub(crate) const MAX_AVERAGING_WINDOW: usize = POST_NU7_AVERAGING_WINDOW;

/// Testnet: a block more than this many spacings after its parent may use the minimum difficulty
const MIN_DIFFICULTY_GAP_MULTIPLIER: i64 = 6;
/// From NU7: 18 × 25 s keeps the 450 s gap (ZIP 218 "Minimum difficulty blocks on Testnet")
const POST_NU7_MIN_DIFFICULTY_GAP_MULTIPLIER: i64 = 18;

impl Params {
    pub fn mainnet() -> Self {
        Self {
            network: NetworkType::Main,
            genesis: display("00040fe8ec8471911baa1db1266ea15dd06b4a8a5c453883c000b031973dce08"),
            blossom: height(653_600),
            nu7: None,
            pow_limit: compact_rounded((U256::one() << 243) - 1),
            pow: true,
            difficulty: Difficulty::Adjusted,
            min_difficulty_from: None,
            // protocol spec §7.6 "block height 2 or greater" (block 1 = genesis + 8.4 h breaks it;
            // zebra's `is_max_block_time_enforced` says every height, never reached under checkpoints)
            max_time_from: Some(height(2)),
        }
    }

    /// Default testnet (`network_upgrade.rs`: min difficulty from 299,188, max time from 653,606)
    pub fn testnet() -> Self {
        Self {
            network: NetworkType::Test,
            genesis: display("05a60a92d99d85997cce3b87616c089f6124d7342af37106edc76126334a2c38"),
            blossom: height(584_000),
            nu7: Some(height(4_465_026)),
            pow_limit: compact_rounded((U256::one() << 251) - 1),
            pow: true,
            difficulty: Difficulty::Adjusted,
            min_difficulty_from: Some(height(299_188)),
            max_time_from: Some(height(653_606)),
        }
    }

    /// Proof of work off (Equihash, hash ≤ target), nBits fixed at the limit; linkage, time and
    /// work still enforced (`testnet.rs` `new_regtest`: a configured testnet, so testnet's
    /// max-time height applies too)
    ///
    /// - `nu7` only moves the spacing (nBits = the limit at every height)
    pub fn regtest(blossom: Height, nu7: Option<Height>) -> Self {
        Self {
            network: NetworkType::Regtest,
            genesis: display("029f11d80ef9765602235e1bc9727e3eb6ba20839319f761fee920d63401e327"),
            blossom,
            nu7,
            pow_limit: compact_rounded(U256::from_big_endian(&[0x0f; 32])),
            pow: false,
            difficulty: Difficulty::Limit,
            min_difficulty_from: None,
            max_time_from: Some(height(653_606)),
        }
    }

    /// Same rules over another genesis (tests building their own chains)
    #[cfg(any(test, feature = "testing"))]
    pub fn with_genesis(self, genesis: BlockHash) -> Self {
        Self { genesis, ..self }
    }

    /// Any valid nBits accepted (model tests: work varies per branch)
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn any_bits(self) -> Self {
        Self { difficulty: Difficulty::Any, ..self }
    }

    pub fn network(&self) -> NetworkType {
        self.network
    }

    pub fn genesis(&self) -> BlockHash {
        self.genesis
    }

    fn nu7_active(&self, at: Height) -> bool {
        self.nu7.is_some_and(|nu7| at >= nu7)
    }

    /// `PoWTargetSpacing(height)`, seconds
    pub(crate) fn spacing(&self, at: Height) -> i64 {
        match (at < self.blossom, self.nu7_active(at)) {
            (_, true) => POST_NU7_SPACING,
            (true, false) => PRE_BLOSSOM_SPACING,
            (false, false) => POST_BLOSSOM_SPACING,
        }
    }

    /// `PoWAveragingWindow(height)` (ZIP 218: every use in §7.7.3 takes the checked height's)
    pub(crate) fn averaging_window(&self, at: Height) -> usize {
        match self.nu7_active(at) {
            true => POST_NU7_AVERAGING_WINDOW,
            false => AVERAGING_WINDOW,
        }
    }

    /// Gap after which a testnet block may drop to the minimum difficulty (`None` = never)
    pub(crate) fn min_difficulty_gap(&self, at: Height) -> Option<i64> {
        let from = self.min_difficulty_from?;
        let multiplier = match self.nu7_active(at) {
            true => POST_NU7_MIN_DIFFICULTY_GAP_MULTIPLIER,
            false => MIN_DIFFICULTY_GAP_MULTIPLIER,
        };
        (at >= from).then(|| self.spacing(at) * multiplier)
    }

    pub(crate) fn max_time_enforced(&self, at: Height) -> bool {
        self.max_time_from.is_some_and(|from| at >= from)
    }

    /// The compact nBits of the limit (regtest's every block, testnet's minimum)
    pub(crate) fn limit_bits(&self) -> u32 {
        to_compact(self.pow_limit)
    }
}

fn compact_rounded(limit: U256) -> U256 {
    expand(to_compact(limit)).expect("a network's limit is a valid target")
}

fn height(height: u32) -> Height {
    Height::try_from(height).expect("activation height in range")
}

/// Display-order hex → internal-order hash
fn display(hex: &str) -> BlockHash {
    let mut bytes = [0u8; 32];
    for (index, byte) in bytes.iter_mut().rev().enumerate() {
        let pair = &hex[index * 2..index * 2 + 2];
        *byte = u8::from_str_radix(pair, 16).expect("constant hex");
    }
    BlockHash::from(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Limits as zcashd's chainparams encode them (`powLimit` → nBits): mainnet 0x1f07ffff,
    /// testnet 0x2007ffff, regtest 0x200f0f0f; genesis hashes render back to their display form;
    /// spacing, window and testnet's minimum-difficulty gap switch at Blossom and NU7
    #[test]
    fn each_network_carries_zcashd_limits_genesis_and_upgrade_switches() {
        let regtest = Params::regtest(height(1), None);
        let bits = [Params::mainnet(), Params::testnet(), regtest].map(|p| p.limit_bits());
        assert_eq!(bits, [0x1f07_ffff, 0x2007_ffff, 0x200f_0f0f]);
        assert_eq!(
            Params::mainnet().genesis().to_string(),
            "00040fe8ec8471911baa1db1266ea15dd06b4a8a5c453883c000b031973dce08"
        );

        let rules = |params: Params, at: u32| {
            let at = height(at);
            (params.spacing(at), params.averaging_window(at), params.min_difficulty_gap(at))
        };
        #[rustfmt::skip]
        let cases = [
            (Params::mainnet(), 653_599,   (150, 17, None)),
            (Params::mainnet(), 653_600,   (75, 17, None)),
            (Params::mainnet(), 9_999_999, (75, 17, None)),
            (Params::testnet(), 299_187,   (150, 17, None)),
            (Params::testnet(), 299_188,   (150, 17, Some(900))),
            (Params::testnet(), 584_000,   (75, 17, Some(450))),
            (Params::testnet(), 4_465_025, (75, 17, Some(450))),
            (Params::testnet(), 4_465_026, (25, 102, Some(450))),
            (Params::regtest(height(1), Some(height(10))), 10, (25, 102, None)),
        ];
        for (params, at, expected) in cases {
            assert_eq!(rules(params, at), expected, "{:?} at {at}", params.network);
        }
    }
}
