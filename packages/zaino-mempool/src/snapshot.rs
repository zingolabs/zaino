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
#[derive(Debug)]
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
    pub txids_sorted: Arc<[TransactionId]>,

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
