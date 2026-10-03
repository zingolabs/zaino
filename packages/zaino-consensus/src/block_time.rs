//! Block-time consensus rules: median-time-past and the candidate-height
//! bracket for a timestamp range.
//!
//! Zcash block timestamps are **not monotonic**. Consensus constrains a block's
//! `nTime` only relative to the *median-time-past* (MTP) of its eleven
//! predecessors, so a later block may legitimately carry an earlier timestamp
//! than the block before it. Selecting blocks by a timestamp range therefore
//! cannot be a plain slice of the height axis.
//!
//! What *is* monotonic is the MTP itself (see [`median_time_past`]). This module
//! turns that into a provably-correct height bracket: [`CandidateSearch`]
//! binary-searches the height axis on MTP and returns the smallest contiguous
//! inclusive height range guaranteed to contain every block whose timestamp lies
//! in `[low, high)`. The caller then filters that bracket by each block's actual
//! time; the filter is exact because the bracket is a guaranteed superset.
//!
//! # The two consensus bounds, as Zebra enforces them
//!
//! For a block at height `h` with median-time-past `MTP(h)` (the median of the
//! times of heights `h-11 ..= h-1`, or of however many predecessors exist near
//! genesis):
//!
//! * **Lower bound — every non-genesis block, every network.** `nTime(h) >
//!   MTP(h)`. Zebra enforces this unconditionally (except for the genesis
//!   block) at
//!   `zebra-state/src/service/check.rs:289` (the `TimeTooEarly` check). It is
//!   *not* gated by any activation height.
//! * **Upper bound — activation-gated.** `nTime(h) <= MTP(h) + 90*60` seconds.
//!   Zebra enforces this only when
//!   `network.is_max_block_time_enforced(h)` is true
//!   (`zebra-state/src/service/check.rs:303`, the `TimeTooLate` check). That
//!   predicate (`zebra-chain/src/parameters/network.rs:241`) is:
//!     * **Mainnet:** true at every height.
//!     * **Testnet:** true only from height
//!       `TESTNET_MAX_TIME_START_HEIGHT = 653_606`
//!       (`zebra-chain/src/parameters/network_upgrade.rs:278`). Below it, a
//!       block's timestamp has no upper bound relative to its MTP.
//!
//! The `90*60`-second constant is `BLOCK_MAX_TIME_SINCE_MEDIAN`
//! (`zebra-state/src/service/check/difficulty.rs:49`) and the eleven-block span
//! is `POW_MEDIAN_BLOCK_SPAN` (`.../check/difficulty.rs:22`).
//!
//! The upper bound is what lets the search tighten the *low* end of the bracket
//! (a block cannot be far above its MTP, so a low MTP rules the block out of a
//! high range). Where the upper bound does not apply — the testnet
//! pre-activation region — that reasoning is invalid and the low end of the
//! bracket must fall back to a full scan down to genesis. [`MaxBlockTimeDrift`]
//! carries the activation boundary so the search knows which regime it is in.

use zaino_primitives::types::{BlockTime, Height, HeightRange};

/// The number of predecessor block times that define a block's
/// median-time-past.
///
/// Zcash's `POW_MEDIAN_BLOCK_SPAN`
/// (`zebra-state/src/service/check/difficulty.rs:22`).
const MEDIAN_BLOCK_SPAN: usize = 11;

/// The consensus upper bound on how far a block's timestamp may exceed the
/// median-time-past of its eleven predecessors.
///
/// A block's `nTime` must satisfy `nTime <= MTP + 90*60` seconds, **where this
/// rule is enforced**. It is a consensus rule, not an empirical observation
/// about how miners behave, which is precisely why the candidate-height search
/// can rely on it rather than on timestamps being "near-monotonic in practice".
///
/// # Activation
///
/// The rule is not active at every height. This type carries the first height
/// at which it is enforced, so a caller can distinguish the two regimes:
///
/// * **Mainnet** ([`MaxBlockTimeDrift::MAINNET`]): enforced at every height, so
///   the activation boundary is [`Height::GENESIS`].
/// * **Testnet** ([`MaxBlockTimeDrift::TESTNET`]): enforced only from
///   `TESTNET_MAX_TIME_START_HEIGHT = 653_606`. Below that height a block's
///   timestamp is unbounded above relative to its MTP, so the candidate bracket
///   cannot be tightened there and must widen to a full scan of the
///   pre-activation region.
///
/// Source: `nTime <= MTP + BLOCK_MAX_TIME_SINCE_MEDIAN`, enforced in
/// `zebra-state/src/service/check.rs:303` and gated by
/// `Network::is_max_block_time_enforced`
/// (`zebra-chain/src/parameters/network.rs:241`); the constant is
/// `BLOCK_MAX_TIME_SINCE_MEDIAN = 90 * 60`
/// (`zebra-state/src/service/check/difficulty.rs:49`); the testnet activation
/// height is `TESTNET_MAX_TIME_START_HEIGHT`
/// (`zebra-chain/src/parameters/network_upgrade.rs:278`).
///
/// Note on the specification text: the Zcash protocol specification phrases the
/// rule as "block height 2 or greater on Mainnet". Zebra's
/// `is_max_block_time_enforced` returns true for *all* mainnet heights, and this
/// module follows Zebra, the authoritative implementation. The difference is
/// immaterial to the bracket: genesis has no MTP and is handled as a boundary
/// case regardless, and the search never relies on the upper bound at height 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MaxBlockTimeDrift {
    /// The first height at which the `nTime <= MTP + drift` rule is enforced.
    enforced_from: Height,
}

impl MaxBlockTimeDrift {
    /// The drift bound in seconds: `90 * 60 = 5400`.
    ///
    /// Zcash's `BLOCK_MAX_TIME_SINCE_MEDIAN`
    /// (`zebra-state/src/service/check/difficulty.rs:49`).
    pub const DRIFT_SECONDS: u32 = 90 * 60;

    /// The testnet activation height of the drift rule,
    /// `TESTNET_MAX_TIME_START_HEIGHT = 653_606`
    /// (`zebra-chain/src/parameters/network_upgrade.rs:278`).
    const TESTNET_ACTIVATION: u32 = 653_606;

    /// The mainnet drift rule: enforced at every height.
    pub const MAINNET: Self = Self {
        enforced_from: Height::GENESIS,
    };

    /// The testnet drift rule: enforced only from height 653_606
    /// (`TESTNET_MAX_TIME_START_HEIGHT`).
    ///
    /// The activation height is const-checked against the protocol maximum at
    /// compile time, so there is no runtime fallback.
    pub const TESTNET: Self = Self {
        enforced_from: Height::from_const(Self::TESTNET_ACTIVATION),
    };

    /// The first height at which the drift bound is enforced on this network.
    ///
    /// Below it, a block's timestamp is unbounded above relative to its MTP.
    pub fn enforced_from(&self) -> Height {
        self.enforced_from
    }

    /// Construct a drift rule with an explicit activation height.
    ///
    /// Only the two named networks occur in production; this exists so tests can
    /// exercise the pre-activation full-scan regime without building a
    /// 653_606-block chain.
    #[cfg(test)]
    const fn activating_at(enforced_from: Height) -> Self {
        Self { enforced_from }
    }
}

/// The median-time-past of a block, given the timestamps of its predecessors.
///
/// Returns the median of the **most recent (trailing) up-to-eleven** entries of
/// `times`, or `None` if `times` is empty (the genesis block, which has no
/// predecessors and no MTP). The median of `n` values is the element at index
/// `n / 2` of the sorted window, matching Zcash's definition
/// `median(S) = sorted(S)[ceil((len(S)+1)/2)]`
/// (`zebra-state/src/service/check/difficulty.rs:365`, `median_time`), and the
/// eleven-entry window is `POW_MEDIAN_BLOCK_SPAN` (`.../difficulty.rs:22`).
///
/// # Monotonicity
///
/// MTP is **non-decreasing** in height, and this is a theorem, not an
/// observation. Going from height `h` to `h+1` the window drops its oldest entry
/// and gains `nTime(h) > MTP(h)` (the lower consensus bound; `>=` suffices).
/// Before the change at most five entries were strictly below `MTP(h)` (it is
/// the sixth smallest of eleven); removing one entry cannot raise that count and
/// the added entry is not below `MTP(h)`, so at most five entries of the new
/// window are below `MTP(h)`. Hence the new sixth-smallest — the new median — is
/// `>= MTP(h)`. In the genesis region the window only grows (nothing is dropped)
/// and the same count argument applies. This monotonicity is what makes the
/// binary search in [`CandidateSearch`] sound.
pub fn median_time_past(times: &[BlockTime]) -> Option<BlockTime> {
    if times.is_empty() {
        return None;
    }
    let window_start = times.len().saturating_sub(MEDIAN_BLOCK_SPAN);
    let mut window: Vec<BlockTime> = times[window_start..].to_vec();
    window.sort_unstable();
    window.get(window.len() / 2).copied()
}

/// A resumable, I/O-free search for the candidate-height bracket of a timestamp
/// range.
///
/// # What it computes
///
/// Given a pinned `tip` and a half-open timestamp range `[low, high)`, it finds
/// the smallest contiguous inclusive [`HeightRange`] guaranteed to contain every
/// block whose `nTime` satisfies `low <= nTime < high`, or `None` if no block
/// can qualify. The caller still filters the bracket by each block's actual
/// time; that filter is exact because the bracket is a guaranteed superset.
///
/// # Why it is a state machine, and how Task 3 drives it
///
/// This crate holds pure consensus logic and must not depend on any async
/// runtime or I/O. The search therefore does not fetch block times itself: it
/// *asks* for them. Each [`CandidateSearch::poll`] returns either
/// [`SearchStep::Need`] — "I need the timestamp at this height" — or
/// [`SearchStep::Done`]. The caller supplies the answer with
/// [`CandidateSearch::supply`] and polls again:
///
/// ```ignore
/// let mut search = CandidateSearch::new(tip, low, high, MaxBlockTimeDrift::MAINNET);
/// let bracket = loop {
///     match search.poll() {
///         SearchStep::Need(h) => {
///             let time = chain_view.header(h).await?.map(|s| s.time);
///             search.supply(h, time);
///         }
///         SearchStep::Done(bracket) => break bracket,
///     }
/// };
/// ```
///
/// zaino-core drives it over its async tier reads (`header(h).await`) with no
/// blocking; tests drive it synchronously from an in-memory map. The logic is
/// identical and fully testable without a runtime.
///
/// # Lookups
///
/// Each MTP evaluation needs up to eleven predecessor timestamps, and the two
/// binary searches (upper bound, then lower bound) probe `O(log n)` heights, so
/// the search requests `O(log n * 11)` timestamps. Answers are cached, so the
/// overlapping windows of successive probes are not re-requested.
#[derive(Clone, Debug)]
pub struct CandidateSearch {
    tip: Height,
    low: BlockTime,
    high: BlockTime,
    drift: MaxBlockTimeDrift,
    cache: Vec<(Height, Option<BlockTime>)>,
    phase: Phase,
}

/// A header the search needed is missing at or below the pinned tip.
///
/// The search only ever requests heights at or below the tip, where a block is
/// guaranteed to exist. A `None` answer for such a height therefore means the
/// caller's view of the chain has a hole, not that the block is absent. Because a
/// missing predecessor would lower the computed median-time-past and could narrow
/// the lower bracket — silently dropping an in-range block — the search refuses to
/// continue and surfaces this error instead of guessing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("missing header at height {height}, at or below the pinned tip")]
pub struct MissingHeader {
    /// The height whose header the caller reported as absent.
    pub height: Height,
}

/// What a [`CandidateSearch`] needs next, or its result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchStep {
    /// The search needs the block timestamp at this height. Supply it with
    /// [`CandidateSearch::supply`]; a height at or below the tip must have a
    /// block, so pass its time, not `None`.
    Need(Height),
    /// The search is complete: the candidate bracket, or `None` if no block can
    /// match the range.
    Done(Option<HeightRange>),
    /// The search cannot complete because a requested header at or below the tip
    /// was missing. See [`MissingHeader`].
    Failed(MissingHeader),
}

/// The internal binary-search state over a contiguous interval of heights.
#[derive(Clone, Copy, Debug)]
struct BinarySearch {
    lo: u32,
    hi: u32,
    mid: u32,
    best: Option<u32>,
}

impl BinarySearch {
    /// A search over the inclusive height interval `[lo, hi]`, which must be
    /// non-empty (`lo <= hi`).
    fn new(lo: u32, hi: u32) -> Self {
        Self {
            lo,
            hi,
            mid: lo.saturating_add((hi.saturating_sub(lo)) / 2),
            best: None,
        }
    }

    fn finished(&self) -> bool {
        self.lo > self.hi
    }

    fn recompute_mid(&mut self) {
        if !self.finished() {
            self.mid = self
                .lo
                .saturating_add((self.hi.saturating_sub(self.lo)) / 2);
        }
    }

    /// Record the predicate at `mid` for a "largest height where the predicate
    /// holds" search (the predicate is true for low heights, false for high).
    fn record_take_highest(&mut self, holds: bool) {
        if holds {
            self.best = Some(self.mid);
            self.lo = self.mid.saturating_add(1);
        } else {
            self.hi = self.mid.saturating_sub(1);
            if self.mid == 0 {
                // Guard the `mid - 1` underflow; the interval is then exhausted.
                self.lo = self.hi.saturating_add(1);
            }
        }
        self.recompute_mid();
    }

    /// Record the predicate at `mid` for a "smallest height where the predicate
    /// holds" search (the predicate is false for low heights, true for high).
    fn record_take_lowest(&mut self, holds: bool) {
        if holds {
            self.best = Some(self.mid);
            self.hi = self.mid.saturating_sub(1);
            if self.mid == 0 {
                self.lo = self.hi.saturating_add(1);
            }
        } else {
            self.lo = self.mid.saturating_add(1);
        }
        self.recompute_mid();
    }
}

/// Which stage of the search is active.
#[derive(Clone, Copy, Debug)]
enum Phase {
    /// `tip == genesis`: the chain is a single block; decide it directly.
    GenesisOnly,
    /// Locating the inclusive top of the bracket (largest height with
    /// `MTP < high`).
    FindHigh(BinarySearch),
    /// Locating the inclusive bottom of the bracket (smallest enforced height
    /// with `MTP >= low - drift`), knowing the already-found top.
    FindLow {
        search: BinarySearch,
        high_bound: Height,
    },
    /// Finished.
    Done(Option<HeightRange>),
    /// Terminated by a missing header at or below the tip.
    Failed(MissingHeader),
}

/// The outcome of attempting to evaluate `MTP(target)` from the cache.
enum MtpAttempt {
    /// A predecessor timestamp has not been supplied yet; the caller must supply
    /// this height.
    NotYetSupplied(Height),
    /// A predecessor at or below the tip was supplied as absent: the chain view
    /// has a hole and the search must fail loud.
    Hole(Height),
    /// `MTP(target)`.
    Ready(BlockTime),
}

impl CandidateSearch {
    /// Begin a search for the candidate bracket of `[low, high)` against a chain
    /// whose highest block is `tip`, under the given drift rule.
    pub fn new(tip: Height, low: BlockTime, high: BlockTime, drift: MaxBlockTimeDrift) -> Self {
        let phase = if low >= high {
            // A half-open empty (or inverted) range holds no block.
            Phase::Done(None)
        } else if tip == Height::GENESIS {
            Phase::GenesisOnly
        } else {
            Phase::FindHigh(BinarySearch::new(1, u32::from(tip)))
        };
        Self {
            tip,
            low,
            high,
            drift,
            cache: Vec::new(),
            phase,
        }
    }

    /// Supply the timestamp at `height` (or `None` if no block exists there).
    pub fn supply(&mut self, height: Height, time: Option<BlockTime>) {
        match self.cache.binary_search_by_key(&height, |(h, _)| *h) {
            Ok(idx) => self.cache[idx].1 = time,
            Err(idx) => self.cache.insert(idx, (height, time)),
        }
    }

    fn cached(&self, height: Height) -> Option<Option<BlockTime>> {
        self.cache
            .binary_search_by_key(&height, |(h, _)| *h)
            .ok()
            .map(|idx| self.cache[idx].1)
    }

    /// Evaluate `MTP(target)` from cached timestamps, or report the first
    /// predecessor height whose timestamp is still missing.
    ///
    /// `target >= 1`, so it always has at least one predecessor.
    fn mtp_of(&self, target: u32) -> MtpAttempt {
        let first_pred = target.saturating_sub(u32::try_from(MEDIAN_BLOCK_SPAN).unwrap_or(0));
        let last_pred = target.saturating_sub(1);
        let mut times: Vec<BlockTime> = Vec::new();
        let mut last_valid: Option<Height> = None;
        for pred in first_pred..=last_pred {
            let Ok(height) = Height::try_from(pred) else {
                // `pred < target <= tip` is always a valid height; an unreachable
                // conversion failure simply contributes no sample.
                continue;
            };
            last_valid = Some(height);
            match self.cached(height) {
                None => return MtpAttempt::NotYetSupplied(height),
                // A predecessor is at or below the tip, so its block must exist; a
                // `None` answer is a hole in the caller's chain view, never a
                // sample to skip. Dropping it would lower the MTP and could narrow
                // the lower bracket.
                Some(None) => return MtpAttempt::Hole(height),
                Some(Some(time)) => times.push(time),
            }
        }
        match median_time_past(&times) {
            Some(mtp) => MtpAttempt::Ready(mtp),
            // `target >= 1` always has a predecessor, so `times` is non-empty and
            // the median exists; treat the unreachable empty case as a hole rather
            // than inventing a value.
            None => MtpAttempt::Hole(last_valid.unwrap_or(Height::GENESIS)),
        }
    }

    /// Advance the search as far as the cache allows, returning the next needed
    /// height or the final result.
    pub fn poll(&mut self) -> SearchStep {
        loop {
            match self.phase {
                Phase::Done(bracket) => return SearchStep::Done(bracket),
                Phase::Failed(missing) => return SearchStep::Failed(missing),
                Phase::GenesisOnly => match self.cached(Height::GENESIS) {
                    None => return SearchStep::Need(Height::GENESIS),
                    // The only height is the tip; its block must exist, so a `None`
                    // answer is a hole, not an out-of-range height.
                    Some(None) => {
                        self.phase = Phase::Failed(MissingHeader {
                            height: Height::GENESIS,
                        });
                    }
                    Some(Some(time)) => {
                        let in_range = self.low <= time && time < self.high;
                        let bracket = in_range.then_some(HeightRange {
                            start: Height::GENESIS,
                            end: Height::GENESIS,
                        });
                        self.phase = Phase::Done(bracket);
                    }
                },
                Phase::FindHigh(mut search) => {
                    if search.finished() {
                        self.start_low_phase(search.best);
                    } else {
                        match self.mtp_of(search.mid) {
                            MtpAttempt::NotYetSupplied(height) => return SearchStep::Need(height),
                            MtpAttempt::Hole(height) => {
                                self.phase = Phase::Failed(MissingHeader { height });
                            }
                            MtpAttempt::Ready(mtp) => {
                                let holds = mtp < self.high;
                                search.record_take_highest(holds);
                                self.phase = Phase::FindHigh(search);
                            }
                        }
                    }
                }
                Phase::FindLow {
                    mut search,
                    high_bound,
                } => {
                    if search.finished() {
                        self.finish_low_phase(search.best, high_bound);
                    } else {
                        match self.mtp_of(search.mid) {
                            MtpAttempt::NotYetSupplied(height) => return SearchStep::Need(height),
                            MtpAttempt::Hole(height) => {
                                self.phase = Phase::Failed(MissingHeader { height });
                            }
                            MtpAttempt::Ready(mtp) => {
                                let threshold =
                                    self.low.saturating_sub(MaxBlockTimeDrift::DRIFT_SECONDS);
                                let holds = mtp >= threshold;
                                search.record_take_lowest(holds);
                                self.phase = Phase::FindLow { search, high_bound };
                            }
                        }
                    }
                }
            }
        }
    }

    /// Transition out of the upper-bound search, given its result.
    fn start_low_phase(&mut self, high_best: Option<u32>) {
        let Some(high_u32) = high_best else {
            // Even `MTP(1)` (which equals the genesis timestamp) is `>= high`, so
            // every block's time is `>= high`: nothing matches.
            self.phase = Phase::Done(None);
            return;
        };
        let high_bound = Height::try_from(high_u32).unwrap_or(self.tip);

        if self.drift.enforced_from() > Height::GENESIS {
            // A pre-activation region exists (testnet): its blocks are unbounded
            // above relative to MTP, so none can be excluded from below. The low
            // end falls back to a full scan to genesis.
            self.finalize(Height::GENESIS, high_bound);
            return;
        }

        // Mainnet: the drift bound tightens the low end. Search `[1, high_bound]`.
        self.phase = Phase::FindLow {
            search: BinarySearch::new(1, high_u32),
            high_bound,
        };
    }

    /// Transition out of the lower-bound search, given its result.
    fn finish_low_phase(&mut self, low_best: Option<u32>, high_bound: Height) {
        let Some(low_u32) = low_best else {
            // No height up to `high_bound` has `MTP >= low - drift`, so every
            // such block has `nTime <= MTP + drift < low`: nothing matches.
            self.phase = Phase::Done(None);
            return;
        };
        // `MTP(1)` equals the genesis timestamp, so when the smallest qualifying
        // height is 1 the genesis block is not provably excluded and must be
        // included; when it is above 1, `MTP(1) < low - drift` proves the genesis
        // timestamp is below `low`, so genesis is excluded.
        let low_bound = if low_u32 <= 1 {
            Height::GENESIS
        } else {
            Height::try_from(low_u32).unwrap_or(Height::GENESIS)
        };
        self.finalize(low_bound, high_bound);
    }

    fn finalize(&mut self, low_bound: Height, high_bound: Height) {
        let bracket = (low_bound <= high_bound).then_some(HeightRange {
            start: low_bound,
            end: high_bound,
        });
        self.phase = Phase::Done(bracket);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const GENESIS_TIME: u32 = 1_477_000_000;

    fn height(h: u32) -> Height {
        Height::try_from(h).expect("test height in range")
    }

    /// Drive a search to completion against a chain whose timestamps are indexed
    /// by height. A requested height past the chain yields `None`.
    fn run(
        tip: u32,
        low: u32,
        high: u32,
        drift: MaxBlockTimeDrift,
        times: &[u32],
    ) -> Option<HeightRange> {
        let mut search = CandidateSearch::new(height(tip), low, high, drift);
        loop {
            match search.poll() {
                SearchStep::Need(h) => {
                    let idx = usize::try_from(u32::from(h)).expect("height fits usize");
                    search.supply(h, times.get(idx).copied());
                }
                SearchStep::Done(bracket) => return bracket,
                SearchStep::Failed(missing) => {
                    panic!("unexpected hole for a complete chain: {missing:?}")
                }
            }
        }
    }

    /// Drive a search, answering `None` for exactly one height (a hole) and the
    /// real time for every other, and return the terminal step.
    fn run_with_hole(mut search: CandidateSearch, times: &[u32], hole: u32) -> SearchStep {
        loop {
            match search.poll() {
                SearchStep::Need(h) => {
                    let requested = u32::from(h);
                    let time = if requested == hole {
                        None
                    } else {
                        let idx = usize::try_from(requested).expect("height fits usize");
                        times.get(idx).copied()
                    };
                    search.supply(h, time);
                }
                terminal => return terminal,
            }
        }
    }

    /// Brute-force reference: every height whose timestamp lies in `[low, high)`.
    fn brute_force(low: u32, high: u32, times: &[u32]) -> Vec<u32> {
        times
            .iter()
            .enumerate()
            .filter(|(_, &t)| low <= t && t < high)
            .map(|(i, _)| u32::try_from(i).expect("index fits u32"))
            .collect()
    }

    /// The heights the bracket yields once filtered by actual time — what the
    /// real consumer ends up serving.
    fn bracket_filtered(
        bracket: Option<HeightRange>,
        low: u32,
        high: u32,
        times: &[u32],
    ) -> Vec<u32> {
        let Some(range) = bracket else {
            return Vec::new();
        };
        let start = u32::from(range.start);
        let end = u32::from(range.end);
        (start..=end)
            .filter(|&h| {
                let idx = usize::try_from(h).expect("height fits usize");
                times.get(idx).is_some_and(|&t| low <= t && t < high)
            })
            .collect()
    }

    /// Build a consensus-valid chain from per-block drift deltas.
    ///
    /// Height 0 is `GENESIS_TIME`; each later block's time is `MTP(h) + delta`
    /// with `delta` in `[1, 5400]`, so every block obeys both bounds
    /// (`MTP < nTime <= MTP + 5400`). Large deltas followed by small ones produce
    /// the non-monotonic (out-of-order) timestamps this search must handle.
    fn chain_from_deltas(deltas: &[u32]) -> Vec<u32> {
        let mut times = vec![GENESIS_TIME];
        for &delta in deltas {
            let predecessors = &times[times.len().saturating_sub(MEDIAN_BLOCK_SPAN)..];
            let mtp = median_time_past(predecessors).expect("non-empty predecessors");
            let clamped = delta.clamp(1, MaxBlockTimeDrift::DRIFT_SECONDS);
            times.push(mtp + clamped);
        }
        times
    }

    // ----- median_time_past unit tests -----

    #[test]
    fn median_of_empty_is_none() {
        assert_eq!(median_time_past(&[]), None);
    }

    #[test]
    fn median_of_single_is_itself() {
        assert_eq!(median_time_past(&[42]), Some(42));
    }

    #[test]
    fn median_picks_index_len_over_two() {
        // Eleven values: sorted index 5 is the sixth smallest.
        let times = [100, 90, 80, 70, 60, 50, 40, 30, 20, 10, 0];
        assert_eq!(median_time_past(&times), Some(50));
    }

    #[test]
    fn median_of_two_takes_the_higher() {
        // len/2 == 1: Zcash's median of an even set is the upper-middle element.
        assert_eq!(median_time_past(&[10, 20]), Some(20));
    }

    #[test]
    fn median_windows_the_trailing_eleven() {
        // A twelve-element slice drops its oldest (first) entry.
        let times = [0, 100, 90, 80, 70, 60, 50, 40, 30, 20, 10, 0];
        assert_eq!(median_time_past(&times), Some(50));
    }

    #[test]
    fn median_is_order_independent_within_the_window() {
        let ascending = [10, 20, 30, 40, 50];
        let shuffled = [30, 10, 50, 20, 40];
        assert_eq!(median_time_past(&ascending), median_time_past(&shuffled));
    }

    // ----- candidate bracket unit tests -----

    #[test]
    fn inverted_range_is_empty() {
        let times = chain_from_deltas(&[60; 20]);
        assert_eq!(run(20, 500, 100, MaxBlockTimeDrift::MAINNET, &times), None);
    }

    #[test]
    fn equal_low_and_high_is_empty() {
        let times = chain_from_deltas(&[60; 20]);
        let point = times[5];
        assert_eq!(
            run(20, point, point, MaxBlockTimeDrift::MAINNET, &times),
            None
        );
    }

    #[test]
    fn range_before_genesis_is_empty() {
        let times = chain_from_deltas(&[60; 20]);
        assert_eq!(
            run(20, 0, GENESIS_TIME, MaxBlockTimeDrift::MAINNET, &times),
            None
        );
    }

    #[test]
    fn range_beyond_tip_is_empty() {
        let times = chain_from_deltas(&[60; 20]);
        let after_tip = times[times.len() - 1] + 10_000;
        assert_eq!(
            run(
                20,
                after_tip,
                after_tip + 10_000,
                MaxBlockTimeDrift::MAINNET,
                &times
            ),
            None
        );
    }

    #[test]
    fn genesis_only_chain_in_range() {
        let times = [GENESIS_TIME];
        assert_eq!(
            run(
                0,
                GENESIS_TIME,
                GENESIS_TIME + 1,
                MaxBlockTimeDrift::MAINNET,
                &times
            ),
            Some(HeightRange {
                start: Height::GENESIS,
                end: Height::GENESIS,
            })
        );
    }

    #[test]
    fn genesis_only_chain_out_of_range() {
        let times = [GENESIS_TIME];
        assert_eq!(
            run(
                0,
                GENESIS_TIME + 1,
                GENESIS_TIME + 2,
                MaxBlockTimeDrift::MAINNET,
                &times
            ),
            None
        );
    }

    #[test]
    fn genesis_region_fewer_than_eleven_predecessors() {
        // A short chain exercises the growing-window MTP near genesis.
        let times = chain_from_deltas(&[60; 5]);
        let low = times[1];
        let high = times[4] + 1;
        let bracket = run(5, low, high, MaxBlockTimeDrift::MAINNET, &times);
        assert_eq!(
            bracket_filtered(bracket, low, high, &times),
            brute_force(low, high, &times)
        );
    }

    #[test]
    fn whole_range_covers_whole_chain() {
        let times = chain_from_deltas(&[300; 40]);
        let bracket = run(40, 0, u32::MAX, MaxBlockTimeDrift::MAINNET, &times);
        assert_eq!(
            bracket_filtered(bracket, 0, u32::MAX, &times),
            brute_force(0, u32::MAX, &times)
        );
    }

    #[test]
    fn fixed_spike_then_low_blocks_is_a_superset() {
        // Eleven steady blocks, a spike at the drift ceiling, then steady blocks
        // whose timestamps fall far below the spike: a deliberate out-of-order
        // sequence. The spike height's time exceeds its successors'.
        let mut deltas = vec![60u32; 12];
        deltas.push(MaxBlockTimeDrift::DRIFT_SECONDS);
        deltas.extend(vec![1u32; 20]);
        let times = chain_from_deltas(&deltas);

        // The spike really does make times non-monotonic.
        let spike_height = 13usize;
        assert!(times[spike_height] > times[spike_height + 1]);

        for &(low, high) in &[
            (times[spike_height] - 1, times[spike_height] + 1),
            (times[spike_height + 1], times[spike_height]),
            (GENESIS_TIME, times[times.len() - 1] + 1),
        ] {
            let bracket = run(
                u32::try_from(times.len() - 1).expect("len fits u32"),
                low,
                high,
                MaxBlockTimeDrift::MAINNET,
                &times,
            );
            assert_eq!(
                bracket_filtered(bracket, low, high, &times),
                brute_force(low, high, &times),
                "mismatch for [{low}, {high})"
            );
        }
    }

    #[test]
    fn equal_timestamps_are_a_superset() {
        // Every block carries the same time (the `nTime >= MTP` boundary). The
        // bracket filtered by time must still equal brute force.
        let times = vec![GENESIS_TIME; 30];
        let tip = u32::try_from(times.len() - 1).expect("len fits u32");
        for &(low, high) in &[
            (GENESIS_TIME, GENESIS_TIME + 1),
            (GENESIS_TIME - 10, GENESIS_TIME),
            (GENESIS_TIME + 1, GENESIS_TIME + 2),
        ] {
            let bracket = run(tip, low, high, MaxBlockTimeDrift::MAINNET, &times);
            assert_eq!(
                bracket_filtered(bracket, low, high, &times),
                brute_force(low, high, &times),
                "mismatch for [{low}, {high})"
            );
        }
    }

    #[test]
    fn testnet_pre_activation_block_is_not_excluded() {
        // A tiny chain under a drift rule activating at height 5. Height 2 is
        // pre-activation and carries a far-future timestamp that the drift bound
        // would otherwise rule out of a high range. The full-scan fallback must
        // keep it.
        let drift = MaxBlockTimeDrift::activating_at(height(5));
        let mut times = chain_from_deltas(&[60; 8]);
        let far_future = GENESIS_TIME + 2_000_000;
        times[2] = far_future;

        let bracket = run(8, far_future, far_future + 1, drift, &times);
        let filtered = bracket_filtered(bracket, far_future, far_future + 1, &times);
        assert!(
            filtered.contains(&2),
            "pre-activation block at height 2 must be in the bracket, got {filtered:?}"
        );
        assert_eq!(filtered, brute_force(far_future, far_future + 1, &times));
    }

    // ----- missing-header (hole) failure tests -----

    #[test]
    fn upper_bound_phase_fails_loud_on_a_hole() {
        // The first upper-bound probe over `[1, 40]` is at height 20, whose MTP
        // window is heights 9..=19. A hole at height 19 is consulted during that
        // probe, so the search must fail rather than compute a narrower MTP.
        let times = chain_from_deltas(&[60; 40]);
        let search = CandidateSearch::new(
            height(40),
            GENESIS_TIME,
            times[40] + 1,
            MaxBlockTimeDrift::MAINNET,
        );
        assert_eq!(
            run_with_hole(search, &times, 19),
            SearchStep::Failed(MissingHeader { height: height(19) })
        );
    }

    #[test]
    fn lower_bound_phase_fails_loud_on_a_hole() {
        // Drive the lower-bound phase in isolation (a hole there is the dangerous
        // one: a dropped predecessor lowers the MTP and would narrow the low end).
        // Its first probe over `[1, 40]` is height 20, window 9..=19; a hole at 19
        // must fail loud, not silently drop an in-range block.
        let times = chain_from_deltas(&[60; 40]);
        let mut search = CandidateSearch::new(
            height(40),
            GENESIS_TIME,
            times[40] + 1,
            MaxBlockTimeDrift::MAINNET,
        );
        search.phase = Phase::FindLow {
            search: BinarySearch::new(1, 40),
            high_bound: height(40),
        };
        assert_eq!(
            run_with_hole(search, &times, 19),
            SearchStep::Failed(MissingHeader { height: height(19) })
        );
    }

    #[test]
    fn genesis_only_fails_loud_on_a_hole() {
        // `tip == genesis` with no block supplied for height 0 is a hole, not an
        // out-of-range answer.
        let search = CandidateSearch::new(
            Height::GENESIS,
            GENESIS_TIME,
            GENESIS_TIME + 1,
            MaxBlockTimeDrift::MAINNET,
        );
        assert_eq!(
            run_with_hole(search, &[GENESIS_TIME], 0),
            SearchStep::Failed(MissingHeader {
                height: Height::GENESIS
            })
        );
    }

    // ----- property tests over random and adversarial chains -----

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(400))]

        /// For a consensus-valid chain and an arbitrary half-open range, the
        /// bracket filtered by actual time equals the brute-force filter. This
        /// is the core correctness property: no in-range block is ever missed.
        #[test]
        fn bracket_matches_brute_force(
            deltas in prop::collection::vec(1u32..=MaxBlockTimeDrift::DRIFT_SECONDS, 0..80),
            low_pick in any::<u32>(),
            width in 0u32..20_000,
        ) {
            let times = chain_from_deltas(&deltas);
            let tip = u32::try_from(times.len() - 1).expect("len fits u32");

            // Choose a range anchored near the chain's own timestamps so the
            // interesting boundaries are well exercised, not just empty ranges.
            let span = times[times.len() - 1].saturating_sub(times[0]).max(1);
            let low = times[0].saturating_sub(10_000) + (low_pick % (span + 20_000));
            let high = low.saturating_add(width);

            let bracket = run(tip, low, high, MaxBlockTimeDrift::MAINNET, &times);
            prop_assert_eq!(
                bracket_filtered(bracket, low, high, &times),
                brute_force(low, high, &times)
            );
        }

        /// The bracket is a genuine superset: every brute-force hit lies inside
        /// the returned inclusive range.
        #[test]
        fn bracket_contains_every_hit(
            deltas in prop::collection::vec(1u32..=MaxBlockTimeDrift::DRIFT_SECONDS, 0..80),
            low in any::<u32>(),
            width in 1u32..50_000,
        ) {
            let times = chain_from_deltas(&deltas);
            let tip = u32::try_from(times.len() - 1).expect("len fits u32");
            let high = low.saturating_add(width);
            let hits = brute_force(low, high, &times);
            let bracket = run(tip, low, high, MaxBlockTimeDrift::MAINNET, &times);

            if hits.is_empty() {
                // Nothing to contain; the bracket may be anything (incl. None).
            } else {
                let range = bracket.expect("non-empty hits require a bracket");
                let start = u32::from(range.start);
                let end = u32::from(range.end);
                for h in hits {
                    prop_assert!(start <= h && h <= end, "hit {h} outside [{start}, {end}]");
                }
            }
        }
    }
}
