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
//! `zaino-mempool-rpc`'s coherence service).

use std::collections::HashMap;
use std::sync::Arc;

use zebra_chain::transaction::Hash as TxHash;

use crate::entry::MempoolEntry;
use crate::ports::BlockRef;

/// Whether the snapshot's transaction set is a complete view of the mempool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MempoolCompleteness {
    /// No set has been built yet.
    NotReady,
    /// The set is a complete view of the source mempool at `source_tip`.
    Complete,
    /// The set is intentionally not complete because a capacity bound was hit.
    /// Full-mempool APIs must not present it as complete.
    IncompleteCapacityLimited,
    /// A source error occurred; the last set is still readable, but the latest
    /// poll could not be applied.
    IncompleteSourceError,
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
    /// ([`MempoolSource::get_mempool_source_tip`](crate::ports::MempoolSource::get_mempool_source_tip)),
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
    pub by_txid: Arc<HashMap<TxHash, Arc<MempoolEntry>>>,

    /// Txids sorted by *reversed* byte order, for shortened-txid suffix lookup.
    pub txids_sorted: Arc<[TxHash]>,

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
}

impl MempoolSnapshot {
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
        }
    }
}
