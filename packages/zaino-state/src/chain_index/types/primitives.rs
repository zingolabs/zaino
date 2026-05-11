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

/// Miner-stamped block timestamp at the consensus-level `u32` width.
///
/// On the wire `nTime` is a 4-byte unsigned little-endian field carrying
/// seconds since the Unix epoch. The protocol places only two consensus
/// constraints on which `u32` values a miner may put there:
///
/// - **Median-Time-Past floor** — strictly greater than the median of the
///   previous 11 blocks' `nTime` (BIP-113, inherited by Zcash).
/// - **Future-time ceiling** — no more than 7200 seconds ahead of the
///   receiving node's network-adjusted time.
///
/// Within those bounds the value is entirely miner-chosen; in particular
/// Stratum-style nTime rolling means it is **not** guaranteed to be the
/// miner's wall clock at block-discovery time. See the *Block Header*
/// section of the [Zcash Protocol
/// Specification](https://zips.z.cash/protocol/protocol.pdf) for the
/// authoritative definition.
///
/// `MinerTime` is the in-memory representation everywhere in the chain
/// index. The only legitimate `i64` for this value is at the zebra/chrono
/// boundary in `helpers::extract_block_data` (where `chrono::DateTime::timestamp()`
/// returns `i64`) and at the on-disk serde layer (encoded as 8 bytes LE
/// for backward compatibility with the v1 schema — see
/// zingolabs/zaino#1102 for the plan to narrow that too).
///
/// Distinct from [`Height`] and [`LogicalTimestamp`] at the type level so
/// callers cannot silently confuse the three despite all three wrapping
/// `u32`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct MinerTime(u32);

impl MinerTime {
    /// Return the underlying `u32` for crossing wire/proto boundaries that
    /// take a bare `u32`. An ergonomic alias for `u32::from(self)`.
    pub(crate) const fn as_u32(self) -> u32 {
        self.0
    }
}

impl From<u32> for MinerTime {
    fn from(value: u32) -> Self {
        Self(value)
    }
}

impl From<MinerTime> for u32 {
    fn from(t: MinerTime) -> Self {
        t.0
    }
}

/// The `i64` produced by `chrono::DateTime::timestamp()` does not fit in
/// the consensus-level `u32` width.
///
/// The only legitimate path that hands a `MinerTime` an `i64` is the
/// zebra/chrono boundary in
/// [`extract_block_data`](crate::chain_index::types::helpers); a value
/// outside `[0, u32::MAX]` there means either a malformed upstream header
/// or a chrono drift bug.
#[derive(Debug, thiserror::Error)]
pub(crate) enum MinerTimeError {
    /// The chrono timestamp could not be narrowed to `u32`.
    #[error(
        "nTime value {0} is outside [0, u32::MAX] — consensus nTime is a 4-byte unsigned field"
    )]
    OutOfU32Range(i64),
}

impl TryFrom<i64> for MinerTime {
    type Error = MinerTimeError;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        u32::try_from(value)
            .map(Self)
            .map_err(|_| MinerTimeError::OutOfU32Range(value))
    }
}

impl std::fmt::Display for MinerTime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// zcashd's strictly-monotonic per-block timestamp.
///
/// Computed by the recurrence
///
/// ```text
/// logical_ts(N) = max(nTime(N), logical_ts(N-1) + 1)
/// ```
///
/// so the resulting sequence is strictly increasing across consecutive
/// blocks even when miner-supplied `nTime` values stall or briefly
/// decrease. The `getblockhashes` RPC filters by `[low, high)` in this
/// space.
///
/// Distinct from [`Height`] and [`MinerTime`] at the type level — all
/// three wrap `u32`, and silently confusing them is a class of latent bug
/// this newtype rules out.
///
/// Not currently persisted; if/when an on-disk index keyed by logical
/// timestamp is added, this type should gain a `ZainoVersionedSerde`
/// impl using the same big-endian encoding as [`Height`] so cursor
/// iteration matches lexicographic order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct LogicalTimestamp(u32);

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
    pub(crate) fn next(previous: Option<Self>, block_time: MinerTime) -> Self {
        let block_time = block_time.as_u32();
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
mod miner_time {
    use super::{MinerTime, MinerTimeError};

    /// `MinerTime` round-trips through its `u32` representation via the
    /// `From` impls. Pins the happy path so a future change that breaks
    /// conversion is loud about it.
    #[test]
    fn u32_round_trips_identity() {
        for raw in [0u32, 1, 1_558_141_697, u32::MAX] {
            assert_eq!(u32::from(MinerTime::from(raw)), raw);
            assert_eq!(MinerTime::from(raw).as_u32(), raw);
        }
    }

    /// `Ord` agrees with the underlying `u32`. Required for any future use
    /// as a `BTreeMap` key or sort key.
    #[test]
    fn ordering_matches_underlying_u32() {
        let a = MinerTime::from(10);
        let b = MinerTime::from(20);
        let c = MinerTime::from(20);
        assert!(a < b);
        assert_eq!(b, c);
        assert!(b <= c);
    }

    /// `TryFrom<i64>` accepts every in-range value — the entire
    /// `[0, u32::MAX]` window is valid consensus `nTime` space.
    #[test]
    fn try_from_i64_accepts_full_u32_range() {
        for raw in [0i64, 1, 1_558_141_697, i64::from(u32::MAX)] {
            let mt = MinerTime::try_from(raw).expect("in-range");
            assert_eq!(u32::from(mt), raw as u32);
        }
    }

    /// Negative `i64` values fail with [`MinerTimeError::OutOfU32Range`].
    /// `nTime` is an unsigned 4-byte field; a negative i64 from chrono
    /// indicates a pre-1970 datetime, which no real header carries.
    #[test]
    fn try_from_i64_rejects_negative() {
        let err = MinerTime::try_from(-1i64).expect_err("negative is out of range");
        assert!(matches!(err, MinerTimeError::OutOfU32Range(-1)));
        let err = MinerTime::try_from(i64::MIN).expect_err("MIN is out of range");
        assert!(matches!(err, MinerTimeError::OutOfU32Range(i64::MIN)));
    }

    /// Values above `u32::MAX` fail with [`MinerTimeError::OutOfU32Range`].
    /// Year-2106 epoch overflow; a real Zcash header cannot represent such
    /// a time on the wire.
    #[test]
    fn try_from_i64_rejects_above_u32_max() {
        let just_above = i64::from(u32::MAX) + 1;
        let err = MinerTime::try_from(just_above).expect_err("u32::MAX + 1 out of range");
        assert!(matches!(err, MinerTimeError::OutOfU32Range(v) if v == just_above));
        let err = MinerTime::try_from(i64::MAX).expect_err("MAX is out of range");
        assert!(matches!(err, MinerTimeError::OutOfU32Range(i64::MAX)));
    }
}

#[cfg(test)]
mod logical_timestamp {
    use super::{LogicalTimestamp, MinerTime};

    fn mt(n: u32) -> MinerTime {
        MinerTime::from(n)
    }

    /// Genesis: with no predecessor the result is just the supplied nTime.
    #[test]
    fn next_with_no_predecessor_returns_block_time() {
        let ts = LogicalTimestamp::next(None, mt(1_558_141_697));
        assert_eq!(ts.as_u32(), 1_558_141_697);
    }

    /// Strictly-greater nTime resets drift to 0 and takes the nTime as-is.
    #[test]
    fn next_with_increasing_block_time_takes_block_time() {
        let prev = LogicalTimestamp::from_u32(1_000);
        let ts = LogicalTimestamp::next(Some(prev), mt(1_075));
        assert_eq!(ts.as_u32(), 1_075);
    }

    /// Equal nTime triggers `+1` — the precise boundary where drift starts.
    /// The recurrence uses `<=`, not `<`, deliberately.
    #[test]
    fn next_with_equal_block_time_increments_by_one() {
        let prev = LogicalTimestamp::from_u32(1_000);
        let ts = LogicalTimestamp::next(Some(prev), mt(1_000));
        assert_eq!(ts.as_u32(), 1_001);
    }

    /// nTime below the running logical_ts continues drift by `+1`,
    /// regardless of how far below.
    #[test]
    fn next_with_lower_block_time_increments_by_one() {
        let prev = LogicalTimestamp::from_u32(1_000);
        let ts = LogicalTimestamp::next(Some(prev), mt(5));
        assert_eq!(ts.as_u32(), 1_001);
    }

    /// At the `u32::MAX` ceiling the increment saturates rather than
    /// wrapping. Cannot happen for real chains (Zcash mainnet ≪ `u32::MAX`
    /// epoch seconds) but the type contract guarantees no overflow panic.
    #[test]
    fn next_saturates_at_u32_max() {
        let prev = LogicalTimestamp::from_u32(u32::MAX);
        let ts = LogicalTimestamp::next(Some(prev), mt(0));
        assert_eq!(ts.as_u32(), u32::MAX);
    }

    /// `Ord` agrees with the underlying `u32`. Required for any future use
    /// as a `BTreeMap` key or sort key.
    #[test]
    fn ordering_matches_underlying_u32() {
        let a = LogicalTimestamp::from_u32(10);
        let b = LogicalTimestamp::from_u32(20);
        let c = LogicalTimestamp::from_u32(20);
        assert!(a < b);
        assert_eq!(b, c);
        assert!(b <= c);
    }

    /// End-to-end recurrence replay: each step is either a reset to nTime
    /// or `prev + 1`, and the resulting sequence is strictly increasing.
    #[test]
    fn recurrence_replay_is_strictly_increasing() {
        // nTimes exercise: reset, reset, +1 (equal), +1 (descending), reset.
        let n_times: [u32; 5] = [100, 105, 105, 50, 200];
        let expected: [u32; 5] = [100, 105, 106, 107, 200];

        let mut prev: Option<LogicalTimestamp> = None;
        for (i, &n_time) in n_times.iter().enumerate() {
            let ts = LogicalTimestamp::next(prev, mt(n_time));
            assert_eq!(ts.as_u32(), expected[i], "step {i}");
            if let Some(p) = prev {
                assert!(p < ts, "step {i} not strictly increasing");
            }
            prev = Some(ts);
        }
    }
}
