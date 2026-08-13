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

use zaino_primitives::types::TransactionId;

use crate::entry::MempoolEntry;
use crate::ports::BlockRef;

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
/// The variants fall into two classes, and the distinction drives whether the
/// tip-aware coherence layer freezes:
///
/// - **Short** — the set is missing transactions Zaino knows about, but what it
///   holds is accurate ([`IncompleteCapacityLimited`](Self::IncompleteCapacityLimited),
///   [`IncompletePendingMetadata`](Self::IncompletePendingMetadata)). Serving it
///   is more useful than withholding it; the missing txids are named in
///   [`MempoolSnapshot::unadmitted`].
/// - **Possibly wrong** — the set may not reflect the source at all
///   ([`IncompleteSourceError`](Self::IncompleteSourceError),
///   [`NotReady`](Self::NotReady)). Only these justify a freeze.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MempoolCompleteness {
    /// No set has been built yet.
    NotReady,
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
    pub fn is_whole(self) -> bool {
        matches!(self, Self::Complete)
    }

    /// Whether the set may not reflect the source, as opposed to merely being
    /// short of transactions Zaino knows it is missing.
    ///
    /// This is the freeze condition: withholding a *short* set adds nothing (the
    /// missing transactions do not appear by hiding the present ones), but a set
    /// that may be *wrong* must not be blessed as coherent.
    pub fn may_be_wrong(self) -> bool {
        matches!(self, Self::NotReady | Self::IncompleteSourceError)
    }
}

/// An immutable, tip-agnostic snapshot of Zaino's mempool read model.
///
/// This mirrors the validator's mempool set as of the last successful poll. It
/// carries no freeze/thaw state — the core never freezes — only the
/// [`source_tip`](Self::source_tip) tag that downstream coherence keys on.
#[derive(Debug)]
#[non_exhaustive]
pub struct MempoolSnapshot {
    /// The validator tip (V) the set was fetched against, if known.
    ///
    /// Sourced from the same fetcher that serves the mempool data
    /// ([`GetMempoolSourceTip`](zaino_source::GetMempoolSourceTip)),
    /// so the set and this tag are a single-source pair. The tip-aware coherence
    /// layer compares this against the non-finalized-state tip to decide
    /// coherence *without* re-fetching — which is only sound because the tag and
    /// the data come from one consistent read.
    pub source_tip: Option<BlockRef>,

    /// Monotonic mempool generation (increments on each published set change).
    pub mempool_generation: u64,

    /// Monotonic event sequence (increments on each published snapshot).
    pub event_sequence: u64,

    /// Entries indexed by txid.
    pub by_txid: Arc<HashMap<TransactionId, Arc<MempoolEntry>>>,

    /// Txids sorted by [`reversed_txid_key`], for shortened-txid suffix lookup.
    ///
    /// Private: this ordering is the precondition the suffix search relies on, so
    /// the field is sealed — set only by [`from_source_set`](Self::from_source_set),
    /// which owns the sort. Read it via [`txids_sorted`](Self::txids_sorted).
    txids_sorted: Arc<[TransactionId]>,

    /// Entries in deterministic (sorted-txid) order, for stable response and
    /// stream startup ordering.
    pub entries_in_order: Arc<[Arc<MempoolEntry>]>,

    /// Number of transactions in the set.
    pub tx_count: usize,

    /// Sum of raw transaction byte lengths.
    pub raw_bytes: u64,

    /// Sum of per-entry ZIP-401 costs (the value bounded by `max_cost_bytes`).
    pub cost_bytes: u64,

    /// Completeness of the transaction set.
    pub completeness: MempoolCompleteness,

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
    pub unadmitted: Arc<HashSet<TransactionId>>,
}

impl MempoolSnapshot {
    /// Build a snapshot from a produced mempool set.
    ///
    /// The only way to construct a populated snapshot. It owns the reversed-key
    /// sort and every accounting total, so the result is internally consistent by
    /// construction: `txids_sorted` is always ordered for the suffix search, and
    /// the counts/bytes always match the entries.
    pub fn from_source_set(
        entries: Vec<Arc<MempoolEntry>>,
        source_tip: Option<BlockRef>,
        completeness: MempoolCompleteness,
        unadmitted: Arc<HashSet<TransactionId>>,
        mempool_generation: u64,
        event_sequence: u64,
    ) -> Self {
        let by_txid: HashMap<TransactionId, Arc<MempoolEntry>> = entries
            .iter()
            .map(|entry| (entry.txid, Arc::clone(entry)))
            .collect();

        let mut txids_sorted: Vec<TransactionId> = by_txid.keys().copied().collect();
        txids_sorted.sort_unstable_by_key(|txid| reversed_txid_key(*txid));

        let entries_in_order: Vec<Arc<MempoolEntry>> = txids_sorted
            .iter()
            .map(|txid| Arc::clone(&by_txid[txid]))
            .collect();

        let tx_count = by_txid.len();
        let raw_bytes = by_txid.values().map(|entry| entry.raw_len).sum();
        let cost_bytes = by_txid.values().map(|entry| entry.cost()).sum();

        Self {
            source_tip,
            mempool_generation,
            event_sequence,
            by_txid: Arc::new(by_txid),
            txids_sorted: Arc::from(txids_sorted),
            entries_in_order: Arc::from(entries_in_order),
            tx_count,
            raw_bytes,
            cost_bytes,
            completeness,
            unadmitted,
        }
    }

    /// Re-publish this set under a new tag / completeness / shortfall without
    /// rebuilding it.
    ///
    /// The entries, order and totals are unchanged, so every collection is reused
    /// and `mempool_generation` held steady — only the tag moved, not the
    /// contents, and bumping the generation would falsely tell the coherence
    /// layer the set changed.
    pub fn retagged(
        &self,
        source_tip: Option<BlockRef>,
        completeness: MempoolCompleteness,
        unadmitted: Arc<HashSet<TransactionId>>,
        event_sequence: u64,
    ) -> Self {
        Self {
            source_tip,
            mempool_generation: self.mempool_generation,
            event_sequence,
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

    /// The set's txids in reversed-key ("canonical") order — the order the
    /// shortened-txid suffix search binary-searches. Returned as the shared
    /// handle so callers can retain it cheaply.
    pub fn txids_sorted(&self) -> &Arc<[TransactionId]> {
        &self.txids_sorted
    }

    /// The initial, empty, not-ready snapshot.
    pub fn empty_not_ready() -> Self {
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
            completeness: MempoolCompleteness::NotReady,
            unadmitted: Arc::new(HashSet::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::MempoolEntry;
    use bytes::Bytes;
    use zaino_primitives::types::Height;

    fn entry(tag: u8) -> Arc<MempoolEntry> {
        let mut id = [0u8; 32];
        id[0] = tag; // dominates natural byte order
        id[31] = 0xFF - tag; // dominates reversed-key order — so the two disagree
        Arc::new(MempoolEntry {
            txid: TransactionId::from(id),
            serialized_tx: Bytes::from_static(b"tx"),
            raw_len: 2,
            entry_height: Height::try_from(2_000_000).expect("valid fixture height"),
            entry_time: None,
            first_seen_generation: 0,
        })
    }

    // The sealed constructor is the only way to build a populated snapshot, and it
    // must establish the suffix-search precondition (reversed-key order) and the
    // accounting itself — regardless of the caller's input order.
    #[test]
    fn from_source_set_sorts_by_reversed_key_and_owns_accounting() {
        let entries = vec![entry(0x30), entry(0x10), entry(0x20)]; // deliberately unordered
        let snapshot = MempoolSnapshot::from_source_set(
            entries.clone(),
            None,
            MempoolCompleteness::Complete,
            Arc::new(HashSet::new()),
            0,
            0,
        );

        assert!(
            snapshot
                .txids_sorted()
                .windows(2)
                .all(|w| reversed_txid_key(w[0]) <= reversed_txid_key(w[1])),
            "txids_sorted must be ordered by reversed_txid_key"
        );
        assert_eq!(snapshot.tx_count, entries.len());
        assert_eq!(snapshot.txids_sorted().len(), entries.len());
        assert_eq!(snapshot.by_txid.len(), entries.len());
        assert_eq!(
            snapshot.cost_bytes,
            entries.iter().map(|entry| entry.cost()).sum::<u64>()
        );
        assert_eq!(
            snapshot.raw_bytes,
            entries.iter().map(|entry| entry.raw_len).sum::<u64>()
        );
    }
}
