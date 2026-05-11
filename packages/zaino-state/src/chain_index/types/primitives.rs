//! Foundational primitive types for the chain index.
//!
//! Business-layer primitives that are *not* persisted directly. DB-serializable
//! primitives (the ones that implement `ZainoVersionedSerde`) live under
//! `types/db/` — this module is reserved for types whose role is purely
//! in-memory / business-logic vocabulary.

use crate::chain_index::types::{BlockHash, Height};

/// The internal `(height, hash)` primitive that uniquely identifies a block.
///
/// Business-layer type. It is neither persisted nor serialized directly —
/// persistence goes through a database-adjacent helper
/// (`PersistentBlockContext` in `types/db/legacy.rs`), and the wire/gRPC
/// boundary converts via `From<proto::BlockId>` (the conversion is the
/// validation step).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlockIndex {
    /// Height of the block.
    pub height: Height,
    /// Hash of the block.
    pub hash: BlockHash,
}

/// zcashd's strictly-monotonic per-block timestamp.
///
/// Computed by the recurrence
///
/// ```text
/// logical_ts(N) = max(nTime(N), logical_ts(N-1) + 1)
/// ```
///
/// so the resulting sequence is strictly increasing across consecutive blocks
/// even when miner-supplied `nTime` values stall or briefly decrease. The
/// `getblockhashes` RPC filters by `[low, high)` in this space.
///
/// `LogicalTimestamp` is intentionally distinct from [`Height`] at the type
/// level — both wrap `u32`, and silently confusing the two is a latent class
/// of bug that this newtype rules out.
///
/// Not currently persisted; if/when an on-disk index keyed by logical
/// timestamp is added, this type should gain a `ZainoVersionedSerde` impl
/// using the same big-endian encoding as [`Height`] so cursor iteration
/// matches lexicographic order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LogicalTimestamp(u32);

impl LogicalTimestamp {
    /// Construct from the raw `u32` value.
    pub(crate) const fn from_u32(value: u32) -> Self {
        Self(value)
    }

    /// Return the underlying `u32`.
    pub(crate) const fn as_u32(self) -> u32 {
        self.0
    }

    /// Compute the next logical timestamp from the predecessor's value and
    /// the new block's miner-stamped `nTime`.
    ///
    /// `previous = None` denotes the genesis block (no predecessor).
    pub(crate) fn next(previous: Option<Self>, block_time: u32) -> Self {
        match previous {
            Some(prev) if block_time <= prev.0 => Self(prev.0.saturating_add(1)),
            _ => Self(block_time),
        }
    }
}

impl std::fmt::Display for LogicalTimestamp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(test)]
mod logical_timestamp {
    use super::LogicalTimestamp;

    /// Genesis: with no predecessor the result is just the supplied nTime.
    #[test]
    fn next_with_no_predecessor_returns_block_time() {
        let ts = LogicalTimestamp::next(None, 1_558_141_697);
        assert_eq!(ts.as_u32(), 1_558_141_697);
    }

    /// Strictly-greater nTime resets drift to 0 and takes the nTime as-is.
    #[test]
    fn next_with_increasing_block_time_takes_block_time() {
        let prev = LogicalTimestamp::from_u32(1_000);
        let ts = LogicalTimestamp::next(Some(prev), 1_075);
        assert_eq!(ts.as_u32(), 1_075);
    }

    /// Equal nTime triggers `+1` — the precise boundary where drift starts.
    /// The recurrence uses `<=`, not `<`, deliberately.
    #[test]
    fn next_with_equal_block_time_increments_by_one() {
        let prev = LogicalTimestamp::from_u32(1_000);
        let ts = LogicalTimestamp::next(Some(prev), 1_000);
        assert_eq!(ts.as_u32(), 1_001);
    }

    /// nTime below the running logical_ts continues drift by `+1`, regardless
    /// of how far below.
    #[test]
    fn next_with_lower_block_time_increments_by_one() {
        let prev = LogicalTimestamp::from_u32(1_000);
        let ts = LogicalTimestamp::next(Some(prev), 5);
        assert_eq!(ts.as_u32(), 1_001);
    }

    /// At the `u32::MAX` ceiling the increment saturates rather than wrapping.
    /// Cannot happen for real chains (Zcash mainnet ≪ `u32::MAX` epoch seconds)
    /// but the type contract guarantees no overflow panic.
    #[test]
    fn next_saturates_at_u32_max() {
        let prev = LogicalTimestamp::from_u32(u32::MAX);
        let ts = LogicalTimestamp::next(Some(prev), 0);
        assert_eq!(ts.as_u32(), u32::MAX);
    }

    /// `Ord` agrees with the underlying `u32`. Required for any future use as
    /// a `BTreeMap` key or sort key.
    #[test]
    fn ordering_matches_underlying_u32() {
        let a = LogicalTimestamp::from_u32(10);
        let b = LogicalTimestamp::from_u32(20);
        let c = LogicalTimestamp::from_u32(20);
        assert!(a < b);
        assert_eq!(b, c);
        assert!(b <= c);
    }

    /// End-to-end recurrence replay: each step is either a reset to nTime or
    /// `prev + 1`, and the resulting sequence is strictly increasing.
    #[test]
    fn recurrence_replay_is_strictly_increasing() {
        // nTimes exercise: reset, reset, +1 (equal), +1 (descending), reset.
        let n_times: [u32; 5] = [100, 105, 105, 50, 200];
        let expected: [u32; 5] = [100, 105, 106, 107, 200];

        let mut prev: Option<LogicalTimestamp> = None;
        for (i, &n_time) in n_times.iter().enumerate() {
            let ts = LogicalTimestamp::next(prev, n_time);
            assert_eq!(ts.as_u32(), expected[i], "step {i}");
            if let Some(p) = prev {
                assert!(p < ts, "step {i} not strictly increasing");
            }
            prev = Some(ts);
        }
    }
}
