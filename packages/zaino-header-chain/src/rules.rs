//! [`validate`]: every consensus rule a header must meet, for an untrusted source (a trusted
//! validator's headers skip it: zebra ran it)
//!
//! - adjustment = zcashd `pow.cpp` (`CalculateNextWorkRequired`) + ZIP 218 (window by height)
//! - time and order = zebra-state `service/check.rs` (`difficulty_threshold_and_time_are_valid`)
//!   + zebra-chain `src/block/header.rs` (`time_is_valid_at`)
//! - cheap rules first, Equihash last (156 µs: a junk header never costs it)

use std::cmp::{max, min};
use std::num::NonZeroU64;

use zaino_primitives::protocol::{median_time_past, MEDIAN_TIME_SPAN};
use zaino_primitives::types::{CompactDifficulty, Target, REGTEST_SOLUTION, STANDARD_SOLUTION};
use zcash_protocol::consensus::BlockHeight;

use crate::header::Header;
use crate::params::{Difficulty, Params, MAX_AVERAGING_WINDOW};

/// Ancestors [`validate`] reads: the largest averaging window, then one more median span below it
pub const CONTEXT: usize = MAX_AVERAGING_WINDOW + MEDIAN_TIME_SPAN;

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
    pub bits: u32,
    pub time: u32,
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
    #[error("parent unknown (fetch it first)")]
    Orphan,
    #[error("parent below the finalized boundary, off the finalized chain")]
    BelowFinalized,
    #[error("cumulative work past 128 bits")]
    WorkOverflow,
    #[error("{held} ancestors held, the rules read {needed}")]
    ShortContext { held: usize, needed: usize },
}

/// Whose refusal: `Sender` = blamed, `Deferred` = valid once the clock passes it (H7, never cached),
/// `Unplaced` = we hold too little to judge it
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    Sender,
    Deferred,
    Unplaced,
}

impl Rejected {
    pub fn fault(&self) -> Fault {
        match self {
            Self::Version { .. }
            | Self::Bits { .. }
            | Self::Difficulty { .. }
            | Self::TimeTooEarly { .. }
            | Self::TimeTooLate { .. }
            | Self::SolutionSize { .. }
            | Self::AboveTarget { .. }
            | Self::Solution
            | Self::WorkOverflow => Fault::Sender,
            Self::FromTheFuture { .. } => Fault::Deferred,
            Self::Orphan | Self::BelowFinalized | Self::ShortContext { .. } => Fault::Unplaced,
        }
    }
}

/// Every consensus rule `header` at `height` must meet: alone (version, nBits, solution, proof of
/// work), then against `ancestors` (newest first, up to `CONTEXT`, empty = genesis) and the clock
///
/// - for an untrusted source only: a trusted validator's headers skip it (zebra ran it)
pub(crate) fn validate(
    params: &Params,
    header: &Header,
    height: BlockHeight,
    ancestors: &[Ancestor],
    now_unix: i64,
) -> Result<(), Rejected> {
    alone(params, header)?;
    in_context(params, header, height, ancestors, now_unix)
}

/// One header's work from its nBits (`2^256 / (target + 1)`)
pub(crate) fn header_work(bits: u32) -> Result<u128, Rejected> {
    difficulty(bits).map(CompactDifficulty::work)
}

fn difficulty(bits: u32) -> Result<CompactDifficulty, Rejected> {
    CompactDifficulty::try_from_bits(bits).map_err(|_| Rejected::Bits { bits })
}

/// What `header` alone decides (version, nBits, solution, proof of work)
pub(crate) fn alone(params: &Params, header: &Header) -> Result<(), Rejected> {
    if header.version() < MIN_VERSION {
        return Err(Rejected::Version { version: header.version() });
    }
    let bits = header.bits();
    let difficulty = difficulty(bits)?;
    let len = header.solution().len();
    let expected = if params.pow { STANDARD_SOLUTION } else { REGTEST_SOLUTION };
    if len != expected {
        return Err(Rejected::SolutionSize { len, expected });
    }
    if params.pow {
        if !difficulty.target().is_met_by(header.hash()) {
            return Err(Rejected::AboveTarget { bits });
        }
        equihash_valid(header)?;
    }
    Ok(())
}

/// `header` at `height` after `context` (ancestors, newest first, empty for genesis) and the clock
///
/// - `context` = every ancestor near genesis, else the window + one median span
///   ([`Rejected::ShortContext`]: the caller holds too few, never the header's fault)
pub(crate) fn in_context(
    params: &Params,
    header: &Header,
    height: BlockHeight,
    context: &[Ancestor],
    now_unix: i64,
) -> Result<(), Rejected> {
    let bits = header.bits();
    let time = header.time();
    let needed = needed_context(params, height);
    if context.len() < needed {
        return Err(Rejected::ShortContext { held: context.len(), needed });
    }

    if let Some(parent) = context.first() {
        let median_time_past =
            median_time_past(context.iter().map(|a| a.time)).expect("a parent holds a time");
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
    Ok(())
}

/// Ancestors [`in_context`] reads at `height`: every one near genesis, else the window + one
/// median span
pub(crate) fn needed_context(params: &Params, height: BlockHeight) -> usize {
    (u32::from(height) as usize).min(params.averaging_window(height) + MEDIAN_TIME_SPAN)
}

/// Equihash (200, 9) over the bytes before the nonce, the nonce, the solution
pub(crate) fn equihash_valid(header: &Header) -> Result<(), Rejected> {
    let (input, nonce) = (header.equihash_input(), header.nonce());
    equihash::is_valid_solution(200, 9, input, &nonce, header.solution())
        .map_err(|_| Rejected::Solution)
}

/// `GetNextWorkRequired` (`None` = any nBits)
///
/// - regtest = the limit
/// - testnet after a long gap = the limit
/// - else [`threshold`] over the window for `height`
pub(crate) fn expected_bits(
    params: &Params,
    height: BlockHeight,
    time: u32,
    parent_time: u32,
    context: &[Ancestor],
) -> Option<u32> {
    match params.difficulty {
        Difficulty::Adjusted => {}
        Difficulty::Limit => return Some(params.limit_bits()),
        #[cfg(test)]
        Difficulty::Any => return None,
    }
    let gap = i64::from(time) - i64::from(parent_time);
    if params.min_difficulty_gap(height).is_some_and(|min_gap| gap > min_gap) {
        return Some(params.limit_bits());
    }
    // - zcashd `pow.cpp`: window walk off genesis (`pindexFirst == NULL`) = the limit outright
    // - zebra 6.x damps a limit mean instead (wrong nBits at mainnet height 1)
    let window = params.averaging_window(height);
    if u32::from(height) as usize <= window {
        return Some(params.limit_bits());
    }
    let targets: Vec<Target> = context[..window]
        .iter()
        .map(|a| difficulty(a.bits).expect("ancestors' nBits were verified").target())
        .collect();
    let mean = Target::mean(&targets).expect("a window holds at least 17 targets");
    let times = |from: usize| context.iter().skip(from).map(|a| a.time);
    let newer = median_time_past(times(0)).expect("a parent holds a time");
    let older = median_time_past(times(window)).expect("height > window: ancestors below it");
    let timespan = i64::from(newer) - i64::from(older);
    Some(threshold(params, mean, timespan, height))
}

/// `CalculateNextWorkRequired(mean, timespan, height)`: the timespan damped by 4, clamped to
/// −16 % / +32 % of the window's, then mean / window timespan × bounded, at most the limit
pub(crate) fn threshold(params: &Params, mean: Target, timespan: i64, height: BlockHeight) -> u32 {
    let window = params.spacing(height) * params.averaging_window(height) as i64;
    // integer division truncates toward zero, as C++ `int64_t /`
    let damped = window + (timespan - window) / DAMPING_FACTOR;
    let lowest = window * (100 - MAX_ADJUST_UP_PERCENT) / 100;
    let highest = window * (100 + MAX_ADJUST_DOWN_PERCENT) / 100;
    let bounded = max(lowest, min(highest, damped));
    let expected = u64::try_from(window).ok().and_then(NonZeroU64::new);
    let actual = u64::try_from(bounded).expect("clamped to at least 84 % of a positive window");
    let threshold = mean.rescale(actual, expected.expect("spacing × window > 0 s"));
    min(params.pow_limit, threshold).to_compact()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn height(h: u32) -> BlockHeight {
        BlockHeight::from(h)
    }

    /// zcashd `src/test/pow_tests.cpp` `CalculateNextWorkRequired` vectors (mean bits, timespan
    /// → bits)
    ///
    /// - pre-Blossom at mainnet height 0
    /// - Blossom variants at mainnet's Blossom height (half the spacing, same clamps)
    /// - testnet NU7 window (102 × 25 s): lower-limit vector → same nBits (same clamp percentages)
    #[test]
    fn threshold_reproduces_zcashds_pow_vectors() {
        let mainnet = Params::mainnet();
        let mean = |bits: u32| difficulty(bits).expect("valid").target();
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
