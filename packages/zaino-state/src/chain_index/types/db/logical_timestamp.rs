//! Database-serializable logical timestamp.
//!
//! [`LogicalTimestamp`] is zcashd's strictly-monotonic per-block timestamp.
//! This module owns both the in-memory recurrence
//! ([`LogicalTimestamp::next`]) and the on-disk byte encoding
//! ([`ZainoVersionedSerde`] impl) — both consumers of the same type, kept
//! together so the recurrence and its persisted form cannot drift apart.
//!
//! ## Encoding
//!
//! 4 bytes big-endian (V1). Big-endian is required so that lexicographic
//! byte order over LMDB keys matches numeric `logical_ts` order, which is
//! the property a cursor-based range scan relies on. Same shape as
//! [`Height`](crate::chain_index::types::Height) for the same reason.

use corez::io::{self, Read, Write};

use crate::chain_index::{
    encoding::{read_u32_be, version, write_u32_be, FixedEncodedLen, ZainoVersionedSerde},
    types::MinerTime,
};

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
/// Distinct from [`Height`](crate::chain_index::types::Height) and
/// [`MinerTime`] at the type level — all three wrap `u32`, and silently
/// confusing them is a class of latent bug this newtype rules out.
///
/// On-disk: 4 bytes big-endian (V1), so cursor scans iterate in
/// ascending logical-ts order.
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

impl ZainoVersionedSerde for LogicalTimestamp {
    const VERSION: u8 = version::V1;

    fn encode_latest<W: Write>(&self, w: &mut W) -> io::Result<()> {
        Self::encode_v1(self, w)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn encode_v1<W: Write>(&self, w: &mut W) -> io::Result<()> {
        // Big-endian so lexicographic byte order matches numeric order —
        // the property cursor-based range scans depend on.
        write_u32_be(w, self.0)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let raw = read_u32_be(r)?;
        Ok(Self(raw))
    }
}

impl FixedEncodedLen for LogicalTimestamp {
    /// 4 bytes (BE u32).
    const ENCODED_LEN: usize = 4;
}

#[cfg(test)]
mod logical_timestamp {
    use super::{LogicalTimestamp, MinerTime, ZainoVersionedSerde};

    fn mt(n: u32) -> MinerTime {
        MinerTime::from(n)
    }

    // ----- Recurrence behaviour -----

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

    // ----- On-disk encoding -----

    /// Byte-for-byte golden: `LogicalTimestamp(1)` is encoded as
    /// `[0x00, 0x00, 0x00, 0x01]` (V1, big-endian). Pins the on-disk
    /// contract so any encoder change that breaks compatibility shows up
    /// as a diff here, not as a silent data-corruption regression in
    /// downstream tables.
    #[test]
    fn encode_v1_is_big_endian_u32() {
        let ts = LogicalTimestamp::from_u32(1);
        let bytes = ts.to_bytes().expect("encode");
        // Expected: 1-byte V1 version tag, then 4 bytes BE u32.
        assert_eq!(
            bytes,
            vec![
                crate::chain_index::encoding::version::V1,
                0x00,
                0x00,
                0x00,
                0x01
            ]
        );
    }

    /// Round-trip across a range of representative values, including the
    /// boundaries.
    #[test]
    fn encode_decode_round_trips() {
        for raw in [0u32, 1, 1_558_141_697, u32::MAX] {
            let ts = LogicalTimestamp::from_u32(raw);
            let bytes = ts.to_bytes().expect("encode");
            let decoded = LogicalTimestamp::from_bytes(&bytes).expect("decode");
            assert_eq!(decoded, ts, "round-trip {raw}");
        }
    }

    /// Lexicographic order over the encoded form matches numeric order
    /// over the underlying `u32`. This is the property LMDB cursor scans
    /// rely on for ascending-range iteration — the whole reason for the
    /// big-endian encoding choice.
    #[test]
    fn encoded_bytes_sort_lexicographically() {
        let values: [u32; 6] = [0, 1, 255, 256, 65_535, 65_536];

        // Encode each, dropping the version tag so we compare body bytes only.
        let bodies: Vec<Vec<u8>> = values
            .iter()
            .map(|&v| {
                let mut bytes = LogicalTimestamp::from_u32(v).to_bytes().expect("encode");
                let _tag = bytes.remove(0);
                bytes
            })
            .collect();

        // Verify each adjacent pair sorts in the expected direction.
        for i in 0..bodies.len() - 1 {
            assert!(
                bodies[i] < bodies[i + 1],
                "encoded {} should sort before encoded {} (bodies: {:?} vs {:?})",
                values[i],
                values[i + 1],
                bodies[i],
                bodies[i + 1],
            );
        }
    }
}
