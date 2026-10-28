//! Writer-local poll state and the capacity-admission ordering key.
//!
//! [`PollState`] is the state the single writer task threads through its polls;
//! [`admission_key`] is the deterministic-but-unpredictable order additions are
//! admitted in when the set is at capacity.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash as _, Hasher as _};
use std::time::Instant;

use zaino_primitives::types::{Height, TransactionId};

/// The order additions are admitted in when the set is at capacity.
///
/// Keyed on validator-assigned metadata first — arrival time, then tip-at-entry
/// height — so a sender cannot buy priority. The txid only breaks ties, and only
/// through a per-process salt, because the timestamp is whole-second granular:
/// without the salt every transaction arriving in the same second would be
/// ordered by raw txid bytes, which the sender *can* grind.
///
/// The honest claim is that admission is **unpredictable to the sender**, not
/// that it is globally fair. An attacker flooding at capacity still lands in the
/// same one-second bucket as the transactions they displace; the salt reduces
/// that from "always wins" to "wins only by luck".
///
/// `entry_time: None` sorts last: an entry the source gave no timestamp for has
/// no claim to priority over one it did.
pub(super) fn admission_key(
    salt: u64,
    entry_time: Option<i64>,
    entry_height: Height,
    txid: &TransactionId,
) -> (bool, i64, u32, u64) {
    let mut hasher = DefaultHasher::new();
    salt.hash(&mut hasher);
    <[u8; 32]>::from(*txid).hash(&mut hasher);
    (
        // `false` sorts before `true`, so "has a timestamp" comes first.
        entry_time.is_none(),
        entry_time.unwrap_or(i64::MAX),
        u32::from(entry_height),
        hasher.finish(),
    )
}

/// State owned by the single writer task across polls.
///
/// Threaded through `&mut` rather than held behind a lock precisely because
/// there is exactly one writer: the poll loop.
#[derive(Default)]
pub(super) struct PollState {
    /// When the metadata listing was last fetched, for the
    /// [`metadata_min_interval`](zaino_mempool::config::MempoolConfig::metadata_min_interval)
    /// floor.
    pub(super) last_metadata_fetch: Option<Instant>,

    /// Transactions the capacity backstop refused, and what each would cost.
    ///
    /// Without this memo they would be rediscovered by the very next diff (which
    /// is recomputed from the held set) and re-fetched forever, hammering the
    /// source while the set stayed capacity-limited. Entries leave the memo when
    /// they leave the source's mempool, or when the set has both fallen below
    /// the low-water mark and freed enough room for that specific transaction —
    /// keeping the cost is what makes the retry decision exact instead of a
    /// guess that can re-refuse in a loop.
    pub(super) refused: HashMap<TransactionId, u64>,

    /// Polls discarded in a row by the tag-stability guard, for the
    /// [`MAX_CONSECUTIVE_DISCARDS`](super::poll::MAX_CONSECUTIVE_DISCARDS) backstop.
    pub(super) consecutive_discards: u32,

    /// Txids this poll saw in the source's listing but did not admit because the
    /// metadata listing was deferred by `metadata_min_interval`.
    ///
    /// Distinct from [`refused`](Self::refused): these are not over the capacity
    /// bound, only waiting for their metadata. Both feed
    /// [`MempoolSnapshot::unadmitted`](zaino_mempool::snapshot::MempoolSnapshot::unadmitted).
    pub(super) deferred: HashSet<TransactionId>,
}

impl PollState {
    /// Every txid the source reported that is not in the published set: refused
    /// by the capacity bound, or deferred awaiting metadata.
    ///
    /// Bounded by the txid-listing cap. Consumers use it to tell "Zaino is short
    /// this transaction, ask again" from "this transaction does not exist".
    pub(super) fn unadmitted(&self) -> HashSet<TransactionId> {
        self.refused
            .keys()
            .copied()
            .chain(self.deferred.iter().copied())
            .collect()
    }
}
