//! The immutable, tip-agnostic mempool snapshot.
//!
//! The core mempool is served as an immutable [`MempoolSnapshot`] published
//! behind an atomic pointer: one writer task swaps in a new snapshot, and many
//! readers clone the `Arc` cheaply. The snapshot is **tip-agnostic** — it mirrors
//! the validator's current mempool set and never freezes — but it is tip-*tagged*:
//! it records [`source_tip`](MempoolSnapshot::source_tip), the validator tip the
//! set was fetched against. That tag is what lets the (optional) tip-aware
//! coherence layer decide, without re-fetching, whether the set is coherent with
//! Zaino's non-finalized-state tip (see the `tip` module and
//! `zaino-mempool-service`'s coherence service).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use zaino_primitives::types::{BlockRef, TransactionId};

use crate::entry::MempoolEntry;

/// The sort key that puts txids in *reversed* byte order.
///
/// The lightwallet exclude filter matches on txid *suffixes*, so sorting by the
/// reversed bytes turns that suffix match into a binary-searchable prefix match.
///
/// One function rather than an inline comparator at each site because the
/// ordering is a contract between two places that must agree: whoever builds
/// [`MempoolSnapshot::txids_sorted`] and whoever binary-searches it. Two
/// independently written comparators that disagree would not fail loudly — the
/// search would simply miss matches.
pub fn reversed_txid_key(txid: TransactionId) -> [u8; 32] {
    let mut bytes = <[u8; 32]>::from(txid);
    bytes.reverse();
    bytes
}

/// Whether the snapshot's transaction set is a complete view of the mempool.
///
/// This describes the fidelity of a set that *exists*. Whether one has been
/// built yet is a separate question, on a separate axis — see
/// [`MempoolSnapshot::is_ready`], and the service's `StatusType`. Mixing the two
/// here would mean every consumer matching on fidelity also had to handle a
/// lifecycle case.
///
/// The variants fall into two classes, and the distinction drives whether the
/// tip-aware coherence layer freezes:
///
/// - **Short** — the set is missing transactions Zaino knows about, but what it
///   holds is accurate ([`IncompleteCapacityLimited`](Self::IncompleteCapacityLimited),
///   [`IncompletePendingMetadata`](Self::IncompletePendingMetadata)). Serving it
///   is more useful than withholding it; the missing txids are named in
///   [`MempoolSnapshot::unadmitted`].
/// - **Possibly wrong** — the set may not reflect the source at all
///   ([`IncompleteSourceError`](Self::IncompleteSourceError)). Only this
///   justifies a freeze.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MempoolCompleteness {
    /// The set is a complete view of the source mempool at `source_tip`.
    Complete,
    /// The set is intentionally not complete because a capacity bound was hit.
    /// Full-mempool APIs must not present it as complete.
    IncompleteCapacityLimited,
    /// Additions were deferred by `metadata_min_interval`: their txids are known
    /// but their validator metadata has not been fetched yet, so they cannot be
    /// admitted. The rest of the poll (removals, tip re-tag) did apply.
    ///
    /// Distinct from [`IncompleteCapacityLimited`](Self::IncompleteCapacityLimited)
    /// so operator telemetry attributes a short set to the right cause — it is
    /// how the `metadata_min_interval` knob is tuned.
    IncompletePendingMetadata,
    /// A source error occurred; the last set is still readable, but the latest
    /// poll could not be applied.
    IncompleteSourceError,
}

impl MempoolCompleteness {
    /// Whether the set is a full view of the source mempool.
    ///
    /// Telemetry and the freeze decision only — do **not** gate reads on this.
    /// A short set still answers positive lookups correctly, and gating negative
    /// lookups on it would make every absent txid unavailable; use
    /// [`MempoolSnapshot::unadmitted`] for the per-txid question instead.
    ///
    /// Says nothing about readiness: the pre-first-poll snapshot is an empty set
    /// that is trivially whole. Pair with [`MempoolSnapshot::is_ready`] where
    /// the question is "is there a set yet".
    pub fn is_whole(self) -> bool {
        matches!(self, Self::Complete)
    }

    /// Whether the set may not reflect the source, as opposed to merely being
    /// short of transactions Zaino knows it is missing.
    ///
    /// One half of the freeze condition: withholding a *short* set adds nothing
    /// (the missing transactions do not appear by hiding the present ones), but
    /// a set that may be *wrong* must not be blessed as coherent. The other half
    /// is readiness — see [`MempoolSnapshot::is_ready`].
    pub fn may_be_wrong(self) -> bool {
        matches!(self, Self::IncompleteSourceError)
    }
}

/// An immutable, tip-agnostic snapshot of Zaino's mempool read model.
///
/// This mirrors the validator's mempool set as of the last successful poll. It
/// carries no freeze/thaw state — the core never freezes — only the
/// [`source_tip`](Self::source_tip) tag that downstream coherence keys on.
///
/// # Construction is sealed
///
/// The fields are private and there are exactly three ways to build one:
/// [`empty`](Self::empty), [`from_entries`](Self::from_entries), and
/// [`retag`](Self::retag). This is not ceremony — the type carries invariants a
/// struct literal cannot express, and violating them fails *silently*:
///
/// - `txids_sorted` must be ordered by [`reversed_txid_key`]. The shortened-txid
///   suffix lookup binary-searches it, so a wrong or stale order does not panic;
///   it simply stops finding matches, and the exclude filter then leaks a txid a
///   client asked to hide.
/// - `by_txid`, `entries_in_order`, `tx_count`, `raw_bytes` and `cost_bytes` all
///   describe the same set and must agree. Drift between them is unobservable
///   until a capacity decision or a `getmempoolinfo` reads the wrong total.
/// - `unadmitted` is empty iff [`completeness`](Self::completeness) is
///   [`Complete`](MempoolCompleteness::Complete).
///
/// `from_entries` owns the sort and derives every total from the entries it is
/// given, so none of these can be got wrong at a call site. Reads stay open via
/// the accessors below.
#[derive(Debug)]
pub struct MempoolSnapshot {
    /// The validator tip (V) the set was fetched against, if known.
    source_tip: Option<BlockRef>,
    /// Monotonic mempool generation (increments on each published set change).
    mempool_generation: u64,
    /// Monotonic event sequence (increments on each published snapshot).
    event_sequence: u64,
    /// Entries indexed by txid.
    by_txid: Arc<HashMap<TransactionId, Arc<MempoolEntry>>>,
    /// Txids sorted by [`reversed_txid_key`], for shortened-txid suffix lookup.
    txids_sorted: Arc<[TransactionId]>,
    /// Entries in deterministic (sorted-txid) order.
    entries_in_order: Arc<[Arc<MempoolEntry>]>,
    /// Number of transactions in the set.
    tx_count: usize,
    /// Sum of raw transaction byte lengths.
    raw_bytes: u64,
    /// Sum of per-entry ZIP-401 costs (the value bounded by `max_cost_bytes`).
    cost_bytes: u64,
    /// Completeness of the transaction set.
    completeness: MempoolCompleteness,
    /// Txids the source reported that are **not** in the set.
    unadmitted: Arc<HashSet<TransactionId>>,
}

impl MempoolSnapshot {
    /// The initial snapshot, before the first poll has run.
    ///
    /// Its [`completeness`](Self::completeness) is
    /// [`Complete`](MempoolCompleteness::Complete), which is not a claim about
    /// the mempool: that variant reads "a complete view of the source mempool
    /// **at `source_tip`**", and `source_tip` is `None` here, so it asserts
    /// nothing. It keeps the enum's invariant exact — `unadmitted` is empty iff
    /// `Complete` — instead of carrying a lifecycle variant that every consumer
    /// matching on fidelity would have to handle.
    ///
    /// "No set yet" is answered by [`is_ready`](Self::is_ready), which reads the
    /// `source_tip` that already encodes it.
    pub fn empty() -> Self {
        Self {
            source_tip: None,
            mempool_generation: 0,
            event_sequence: 0,
            by_txid: Arc::new(HashMap::new()),
            txids_sorted: Arc::from([]),
            entries_in_order: Arc::from([]),
            tx_count: 0,
            raw_bytes: 0,
            cost_bytes: 0,
            completeness: MempoolCompleteness::Complete,
            unadmitted: Arc::new(HashSet::new()),
        }
    }

    /// The next published set, built from the entries it should hold.
    ///
    /// Owns the [`reversed_txid_key`] ordering and derives `entries_in_order`,
    /// `tx_count`, `raw_bytes` and `cost_bytes` from `by_txid`, so the derived
    /// state cannot disagree with the set. Totals are summed here rather than
    /// carried forward incrementally: the sort is already `O(n log n)` over the
    /// same entries, so one more `O(n)` pass is noise beside it, and it makes a
    /// drifted running total unrepresentable rather than merely unlikely.
    ///
    /// Advances both counters relative to `previous` — the generation because
    /// the contents changed, the sequence because a snapshot was published. Use
    /// [`retag`](Self::retag) when the contents did *not* change.
    ///
    /// # Panics
    ///
    /// Debug builds only, on `unadmitted` non-empty while `completeness` is
    /// `Complete` — a contradiction the publisher would otherwise ship silently.
    pub fn from_entries(
        previous: &Self,
        by_txid: HashMap<TransactionId, Arc<MempoolEntry>>,
        source_tip: Option<BlockRef>,
        completeness: MempoolCompleteness,
        unadmitted: HashSet<TransactionId>,
    ) -> Self {
        debug_assert!(
            unadmitted.is_empty() || completeness != MempoolCompleteness::Complete,
            "a Complete set cannot be short of {} txid(s)",
            unadmitted.len(),
        );

        let mut txids_sorted: Vec<_> = by_txid.keys().copied().collect();
        txids_sorted.sort_unstable_by_key(|txid| reversed_txid_key(*txid));

        let entries_in_order: Vec<_> = txids_sorted
            .iter()
            .map(|txid| Arc::clone(&by_txid[txid]))
            .collect();

        let raw_bytes = entries_in_order.iter().map(|entry| entry.raw_len).sum();
        let cost_bytes = entries_in_order.iter().map(|entry| entry.cost()).sum();

        Self {
            source_tip,
            mempool_generation: previous.mempool_generation.saturating_add(1),
            event_sequence: previous.event_sequence.saturating_add(1),
            tx_count: by_txid.len(),
            by_txid: Arc::new(by_txid),
            txids_sorted: Arc::from(txids_sorted),
            entries_in_order: Arc::from(entries_in_order),
            raw_bytes,
            cost_bytes,
            completeness,
            unadmitted: Arc::new(unadmitted),
        }
    }

    /// Re-stamp this snapshot's tag and completeness, keeping its set.
    ///
    /// For a publication that carries no delta. The collections are shared by
    /// refcount rather than rebuilt, and — crucially — `mempool_generation` is
    /// **held**: bumping it on an unchanged set would tell the coherence layer
    /// the contents moved and make it redo its work on every tip re-tag. The
    /// event sequence still advances, because a snapshot was published.
    ///
    /// # Panics
    ///
    /// As [`from_entries`](Self::from_entries).
    pub fn retag(
        &self,
        source_tip: Option<BlockRef>,
        completeness: MempoolCompleteness,
        unadmitted: Arc<HashSet<TransactionId>>,
    ) -> Self {
        debug_assert!(
            unadmitted.is_empty() || completeness != MempoolCompleteness::Complete,
            "a Complete set cannot be short of {} txid(s)",
            unadmitted.len(),
        );

        Self {
            source_tip,
            mempool_generation: self.mempool_generation,
            event_sequence: self.event_sequence.saturating_add(1),
            by_txid: Arc::clone(&self.by_txid),
            txids_sorted: Arc::clone(&self.txids_sorted),
            entries_in_order: Arc::clone(&self.entries_in_order),
            tx_count: self.tx_count,
            raw_bytes: self.raw_bytes,
            cost_bytes: self.cost_bytes,
            completeness,
            unadmitted,
        }
    }

    /// The validator tip (V) the set was fetched against, if known.
    ///
    /// Sourced from the same fetcher that serves the mempool data
    /// ([`GetMempoolSourceTip`](zaino_source::GetMempoolSourceTip)), so the set
    /// and this tag are a single-source pair. The tip-aware coherence layer
    /// compares this against the non-finalized-state tip to decide coherence
    /// *without* re-fetching — which is only sound because the tag and the data
    /// come from one consistent read.
    pub fn source_tip(&self) -> Option<BlockRef> {
        self.source_tip
    }

    /// Monotonic mempool generation (increments on each published set change).
    pub fn mempool_generation(&self) -> u64 {
        self.mempool_generation
    }

    /// Monotonic event sequence (increments on each published snapshot).
    pub fn event_sequence(&self) -> u64 {
        self.event_sequence
    }

    /// Entries indexed by txid.
    pub fn by_txid(&self) -> &HashMap<TransactionId, Arc<MempoolEntry>> {
        &self.by_txid
    }

    /// Txids ordered by [`reversed_txid_key`], for shortened-txid suffix lookup.
    ///
    /// The ordering is a precondition of that lookup's binary search; it is
    /// established once, by [`from_entries`](Self::from_entries).
    pub fn txids_sorted(&self) -> &Arc<[TransactionId]> {
        &self.txids_sorted
    }

    /// Entries in deterministic (sorted-txid) order, for stable response and
    /// stream startup ordering.
    pub fn entries_in_order(&self) -> &Arc<[Arc<MempoolEntry>]> {
        &self.entries_in_order
    }

    /// Number of transactions in the set.
    pub fn tx_count(&self) -> usize {
        self.tx_count
    }

    /// Sum of raw transaction byte lengths.
    pub fn raw_bytes(&self) -> u64 {
        self.raw_bytes
    }

    /// Sum of per-entry ZIP-401 costs (the value bounded by `max_cost_bytes`).
    pub fn cost_bytes(&self) -> u64 {
        self.cost_bytes
    }

    /// Completeness of the transaction set.
    pub fn completeness(&self) -> MempoolCompleteness {
        self.completeness
    }

    /// Txids the source reported that are **not** in [`by_txid`](Self::by_txid):
    /// refused by the capacity bound, or deferred awaiting their metadata.
    ///
    /// This is the per-txid form of [`completeness`](Self::completeness), and the
    /// one reads should consult. It lets a caller distinguish "Zaino is short
    /// this transaction, ask again" from "this transaction does not exist" —
    /// without that distinction a short set has to answer *every* absent txid as
    /// unavailable, which is the common case and far worse than the rare wrong
    /// answer it would be avoiding.
    ///
    /// Bounded by the source's txid-listing cap. Empty whenever the set is
    /// [`Complete`](MempoolCompleteness::Complete).
    pub fn unadmitted(&self) -> &Arc<HashSet<TransactionId>> {
        &self.unadmitted
    }

    /// Whether a poll has ever populated this snapshot.
    ///
    /// Derived from [`source_tip`](Self::source_tip) rather than stored: a set
    /// is tagged with the validator tip it was read at, so having no tag *is*
    /// having no set. Every publication path stamps a tag, so this is `false`
    /// only for [`empty`](Self::empty).
    ///
    /// An unready snapshot must not be served as a tip-coherent view — an empty
    /// mempool presented as authoritative would tell a caller their transaction
    /// is not pending when Zaino simply has not looked yet.
    pub fn is_ready(&self) -> bool {
        self.source_tip.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use zaino_primitives::types::Height;

    fn entry(txid_byte: u8, raw_len: u64) -> (TransactionId, Arc<MempoolEntry>) {
        let txid = TransactionId::from([txid_byte; 32]);
        (
            txid,
            Arc::new(MempoolEntry {
                txid,
                serialized_tx: Bytes::from(vec![0u8; raw_len as usize]),
                raw_len,
                entry_height: Height::try_from(100u32).expect("valid fixture height"),
                entry_time: Some(1_700_000_000),
                first_seen_generation: 0,
            }),
        )
    }

    fn tip() -> BlockRef {
        BlockRef {
            hash: zaino_primitives::types::BlockHash::from([9u8; 32]),
            height: Height::try_from(100u32).expect("valid fixture height"),
        }
    }

    /// The ordering the suffix search depends on is established by the
    /// constructor, not by the caller — so no insertion order can produce a
    /// snapshot whose `txids_sorted` is wrong. This is the invariant that used
    /// to be a call-site convention.
    #[test]
    fn from_entries_orders_by_reversed_txid_regardless_of_insertion_order() {
        // Inserted in an order that is neither sorted nor reverse-sorted, and a
        // `HashMap` iteration order is arbitrary regardless.
        let by_txid: HashMap<_, _> = [entry(0x40, 10), entry(0x10, 10), entry(0x90, 10)]
            .into_iter()
            .collect();

        let snapshot = MempoolSnapshot::from_entries(
            &MempoolSnapshot::empty(),
            by_txid,
            Some(tip()),
            MempoolCompleteness::Complete,
            HashSet::new(),
        );

        let keys: Vec<_> = snapshot
            .txids_sorted()
            .iter()
            .map(|txid| reversed_txid_key(*txid))
            .collect();
        assert!(
            keys.windows(2).all(|pair| pair[0] <= pair[1]),
            "txids_sorted must be ordered by reversed_txid_key, got {keys:?}"
        );

        // `entries_in_order` follows the same order, so the two cannot disagree.
        let entry_order: Vec<_> = snapshot
            .entries_in_order()
            .iter()
            .map(|entry| reversed_txid_key(entry.txid))
            .collect();
        assert_eq!(entry_order, keys);
    }

    /// Totals are derived from the set rather than supplied, so they cannot
    /// drift from the entries they describe.
    #[test]
    fn from_entries_derives_totals_from_the_set() {
        let by_txid: HashMap<_, _> = [entry(0x01, 500), entry(0x02, 40_000)]
            .into_iter()
            .collect();

        let snapshot = MempoolSnapshot::from_entries(
            &MempoolSnapshot::empty(),
            by_txid,
            Some(tip()),
            MempoolCompleteness::Complete,
            HashSet::new(),
        );

        assert_eq!(snapshot.tx_count(), 2);
        assert_eq!(snapshot.raw_bytes(), 40_500);
        // The 500-byte entry is floored to the ZIP-401 threshold; the other is
        // charged its real size.
        assert_eq!(
            snapshot.cost_bytes(),
            crate::config::MEMPOOL_TRANSACTION_COST_THRESHOLD + 40_000
        );
    }

    /// A re-tag republishes the same set: the generation must hold, or the
    /// coherence layer treats every tip re-stamp as new contents and redoes its
    /// work. The event sequence still advances, because something was published.
    #[test]
    fn retag_holds_the_generation_and_advances_the_sequence() {
        let by_txid: HashMap<_, _> = [entry(0x01, 10)].into_iter().collect();
        let published = MempoolSnapshot::from_entries(
            &MempoolSnapshot::empty(),
            by_txid,
            Some(tip()),
            MempoolCompleteness::Complete,
            HashSet::new(),
        );

        let retagged = published.retag(
            Some(tip()),
            MempoolCompleteness::IncompleteSourceError,
            Arc::clone(published.unadmitted()),
        );

        assert_eq!(
            retagged.mempool_generation(),
            published.mempool_generation()
        );
        assert_eq!(
            retagged.event_sequence(),
            published.event_sequence().saturating_add(1)
        );
        // The set is shared, not rebuilt.
        assert_eq!(retagged.tx_count(), published.tx_count());
        assert_eq!(retagged.cost_bytes(), published.cost_bytes());
    }

    /// The initial snapshot is `Complete` but not *ready*: the two questions sit
    /// on separate axes, and only the second gates a coherent read.
    #[test]
    fn the_empty_snapshot_is_complete_but_not_ready() {
        let snapshot = MempoolSnapshot::empty();
        assert!(snapshot.completeness().is_whole());
        assert!(!snapshot.is_ready());
        assert!(!snapshot.completeness().may_be_wrong());
    }

    /// `unadmitted` non-empty contradicts `Complete`; the constructor refuses to
    /// publish the contradiction rather than shipping it silently.
    #[test]
    #[should_panic(expected = "Complete set cannot be short")]
    fn a_complete_set_cannot_be_short() {
        let short = HashSet::from([TransactionId::from([0xEE; 32])]);
        let _ = MempoolSnapshot::from_entries(
            &MempoolSnapshot::empty(),
            HashMap::new(),
            Some(tip()),
            MempoolCompleteness::Complete,
            short,
        );
    }
}
