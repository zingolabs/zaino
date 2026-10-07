//! Header rules: stage A (the header alone) and stage B (against its ancestors and the clock)
//!
//! - adjustment = zcashd `pow.cpp` (`CalculateNextWorkRequired`) + ZIP 218 (window by height)
//! - time and order = zebra-state `src/service/check.rs` (`difficulty_threshold_and_time_are_valid`)
//!   + zebra-chain `src/block/header.rs` (`time_is_valid_at`)
//! - stage A: cheap rules first, Equihash last (156 µs: a junk header never costs it)

use std::cmp::{max, min};

use zaino_primitives::types::{Height, REGTEST_SOLUTION, STANDARD_SOLUTION};
use zcash_protocol::consensus::NetworkType;

use crate::header::Header;
use crate::params::{Difficulty, Params, MAX_AVERAGING_WINDOW};
use crate::target::{expand, mean, meets, to_compact, work, U256};

pub(crate) const MEDIAN_SPAN: usize = 11;
/// Ancestors the rules read: the largest averaging window, then one more median span below it
pub(crate) const CONTEXT: usize = MAX_AVERAGING_WINDOW + MEDIAN_SPAN;

const DAMPING_FACTOR: i64 = 4;
const MAX_ADJUST_UP_PERCENT: i64 = 16;
const MAX_ADJUST_DOWN_PERCENT: i64 = 32;
/// Mainnet (and testnet from 653,606): no later than median time past + 90 min
const MAX_TIME_SINCE_MEDIAN: i64 = 90 * 60;
/// No later than the local clock + 2 h
pub(crate) const MAX_FUTURE: i64 = 2 * 60 * 60;
const MIN_VERSION: i32 = 4;

/// One ancestor as the rules read it
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Ancestor {
    pub(crate) bits: u32,
    pub(crate) time: u32,
}

/// Which rule refused a header (`Ok` = it extends its parent validly)
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Rejected {
    #[error("version {version} below 4 (int32: high bit = negative)")]
    Version { version: i32 },
    #[error("nBits {bits:#010x} is not a valid compact target")]
    Bits { bits: u32 },
    #[error("nBits {bits:#010x}, the adjustment expects {expected:#010x}")]
    Difficulty { bits: u32, expected: u32 },
    #[error("time {time} not after the median time past {median_time_past}")]
    TimeTooEarly { time: u32, median_time_past: u32 },
    #[error("time {time} past the median time past + 90 min ({max})")]
    TimeTooLate { time: u32, max: i64 },
    #[error("time {time} past the local clock + 2 h ({max})")]
    FromTheFuture { time: u32, max: i64 },
    #[error("solution of {len} bytes on a network expecting {expected}")]
    SolutionSize { len: usize, expected: usize },
    #[error("hash above the target nBits {bits:#010x} encodes")]
    AboveTarget { bits: u32 },
    #[error("Equihash solution invalid")]
    Solution,
    #[error("prev_hash is not the hash of the header before it in the run")]
    Unlinked,
    #[error("genesis is not this network's")]
    WrongGenesis,
    #[error("parent unknown (fetch it first)")]
    Orphan,
    #[error("parent below the final boundary, off the final chain")]
    BelowFinal,
    #[error("cumulative work past 128 bits")]
    WorkOverflow,
}

impl Rejected {
    /// Time-bound only: valid once the clock passes it (H7: deferred, never blamed or cached)
    pub fn is_deferred(&self) -> bool {
        matches!(self, Self::FromTheFuture { .. })
    }
}

/// Header past stage A under one network's rules: only these enter a [`HeaderChain`]
///
/// [`HeaderChain`]: crate::HeaderChain
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    header: Header,
    network: NetworkType,
}

impl Checked {
    pub(crate) fn header(&self) -> &Header {
        &self.header
    }

    pub(crate) fn network(&self) -> NetworkType {
        self.network
    }
}

/// Stage A: what `header` alone decides (version, nBits, solution, proof of work); pure, parallel
pub fn check(params: &Params, header: Header) -> Result<Checked, Rejected> {
    if header.version() < MIN_VERSION {
        return Err(Rejected::Version { version: header.version() });
    }
    let bits = header.bits();
    let target = expand(bits).ok_or(Rejected::Bits { bits })?;
    work(target).ok_or(Rejected::Bits { bits })?;
    let len = header.solution().len();
    let expected = if params.pow { STANDARD_SOLUTION } else { REGTEST_SOLUTION };
    if len != expected {
        return Err(Rejected::SolutionSize { len, expected });
    }
    if params.pow {
        if !meets(header.hash().into(), target) {
            return Err(Rejected::AboveTarget { bits });
        }
        equihash_valid(&header)?;
    }
    Ok(Checked { header, network: params.network })
}

/// Stage A's last step: each `prev_hash` = the hash before it; the run cut at its first failure
pub fn link_run(
    checked: impl IntoIterator<Item = Result<Checked, Rejected>>,
) -> (Vec<Checked>, Option<Rejected>) {
    let mut run: Vec<Checked> = Vec::new();
    for header in checked {
        let header = match header {
            Ok(header) => header,
            Err(rejected) => return (run, Some(rejected)),
        };
        if run.last().is_some_and(|last| last.header.hash() != header.header.prev_hash()) {
            return (run, Some(Rejected::Unlinked));
        }
        run.push(header);
    }
    (run, None)
}

/// Stage B: `header` at `height` after `context` (ancestors, newest first; empty for genesis)
///
/// - returns the header's own work (`2^256 / (target + 1)`)
pub(crate) fn in_context(
    params: &Params,
    header: &Header,
    height: Height,
    context: &[Ancestor],
    now_unix: i64,
) -> Result<u128, Rejected> {
    let bits = header.bits();
    let own = expand(bits).and_then(work).expect("stage A checked nBits");
    let time = header.time();

    if let Some(parent) = context.first() {
        let median_time_past = median_time(context.iter().take(MEDIAN_SPAN).map(|a| a.time));
        if time <= median_time_past {
            return Err(Rejected::TimeTooEarly { time, median_time_past });
        }
        let latest = i64::from(median_time_past) + MAX_TIME_SINCE_MEDIAN;
        if params.max_time_enforced(height) && i64::from(time) > latest {
            return Err(Rejected::TimeTooLate { time, max: latest });
        }
        let expected = expected_bits(params, height, time, parent.time, context);
        if let Some(expected) = expected.filter(|expected| *expected != bits) {
            return Err(Rejected::Difficulty { bits, expected });
        }
    }
    let horizon = now_unix + MAX_FUTURE;
    if i64::from(time) > horizon {
        return Err(Rejected::FromTheFuture { time, max: horizon });
    }
    Ok(own)
}

/// Equihash (200, 9) over the bytes before the nonce, the nonce, the solution
pub(crate) fn equihash_valid(header: &Header) -> Result<(), Rejected> {
    let (input, nonce) = (header.equihash_input(), header.nonce());
    equihash::is_valid_solution(200, 9, input, &nonce, header.solution())
        .map_err(|_| Rejected::Solution)
}

/// `GetNextWorkRequired` (`None` = any nBits): regtest = the limit; testnet after a long gap =
/// the limit; else [`threshold`] over the window for `height`
pub(crate) fn expected_bits(
    params: &Params,
    height: Height,
    time: u32,
    parent_time: u32,
    context: &[Ancestor],
) -> Option<u32> {
    match params.difficulty {
        Difficulty::Adjusted => {}
        Difficulty::Limit => return Some(params.limit_bits()),
        #[cfg(any(test, feature = "testing"))]
        Difficulty::Any => return None,
    }
    let gap = i64::from(time) - i64::from(parent_time);
    if params.min_difficulty_gap(height).is_some_and(|min_gap| gap > min_gap) {
        return Some(params.limit_bits());
    }
    // zcashd `pow.cpp`: the window walk off genesis (`pindexFirst == NULL`) = the limit outright;
    // zebra 6.x damps a limit mean instead, which mints the wrong nBits at mainnet height 1
    let window = params.averaging_window(height);
    if context.len() <= window {
        return Some(params.limit_bits());
    }
    let targets: Vec<U256> = context[..window]
        .iter()
        .map(|a| expand(a.bits).expect("ancestors' nBits were verified"))
        .collect();
    let newer = median_time(context.iter().take(MEDIAN_SPAN).map(|a| a.time));
    let older = median_time(context.iter().skip(window).take(MEDIAN_SPAN).map(|a| a.time));
    let timespan = i64::from(newer) - i64::from(older);
    Some(threshold(params, mean(&targets), timespan, height))
}

/// `CalculateNextWorkRequired(mean, timespan, height)`: the timespan damped by 4, clamped to
/// −16 % / +32 % of the window's, then mean / window timespan × bounded, at most the limit
pub(crate) fn threshold(params: &Params, mean: U256, timespan: i64, height: Height) -> u32 {
    let window = params.spacing(height) * params.averaging_window(height) as i64;
    // integer division truncates toward zero, as C++ `int64_t /`
    let damped = window + (timespan - window) / DAMPING_FACTOR;
    let lowest = window * (100 - MAX_ADJUST_UP_PERCENT) / 100;
    let highest = window * (100 + MAX_ADJUST_DOWN_PERCENT) / 100;
    let bounded = max(lowest, min(highest, damped));
    let threshold = (mean / U256::from(window)) * U256::from(bounded);
    to_compact(min(params.pow_limit, threshold))
}

/// `median_time`: the upper median (index len / 2 of the sorted times)
pub(crate) fn median_time(times: impl Iterator<Item = u32>) -> u32 {
    let mut times: Vec<u32> = times.collect();
    times.sort_unstable();
    times[times.len() / 2]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn height(h: u32) -> Height {
        Height::try_from(h).expect("in range")
    }

    /// zcashd `src/test/pow_tests.cpp` `CalculateNextWorkRequired` vectors (mean bits, timespan
    /// → bits), pre-Blossom at mainnet height 0; the Blossom variants at mainnet's Blossom height
    /// (half the spacing, same clamps); a testnet NU7 window (102 × 25 s) clamps on the same
    /// percentages, so the lower-limit vector lands on the same nBits there too
    #[test]
    fn threshold_reproduces_zcashds_pow_vectors() {
        let mainnet = Params::mainnet();
        let mean = |bits: u32| expand(bits).expect("valid");
        #[rustfmt::skip]
        let cases = [
            ("get_next_work",                   mainnet,            0,         0x1d00_ffff, 3_570,      0x1d01_1998),
            ("get_next_work_pow_limit",         mainnet,            0,         0x1f07_ffff, 2_055_491,  0x1f07_ffff),
            ("get_next_work_lower_limit",       mainnet,            0,         0x1c05_a3f4, -899_999_083, 0x1c04_bceb),
            ("get_next_work_upper_limit",       mainnet,            0,         0x1c38_7f6f, 5_815,      0x1c4a_93bb),
            ("lower_limit_actual_blossom",      mainnet,            653_600,   0x1c05_a3f4, 458,        0x1c04_bceb),
            ("pow_limit_blossom",               mainnet,            653_600,   0x1f07_ffff, 2_055_491,  0x1f07_ffff),
            ("lower limit, NU7 window",         Params::testnet(),  4_465_026, 0x1c05_a3f4, 0,          0x1c04_bceb),
        ];
        for (case, params, at, bits, timespan, expected) in cases {
            let threshold = threshold(&params, mean(bits), timespan, height(at));
            assert_eq!(threshold, expected, "{case}: {threshold:#010x}");
        }
        let blossom = threshold(&mainnet, mean(0x1d00_ffff), 1_445, height(653_600));
        assert!(blossom < 0x1d01_1998, "get_next_work_blossom: {blossom:#010x}");
    }
}
