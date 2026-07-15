//! The immutable mempool snapshot and its tip-coherence metadata.
//!
//! The mempool is served as an immutable [`MempoolSnapshot`] published behind an
//! atomic pointer: one writer task swaps in a new snapshot, and many readers
//! clone the `Arc` cheaply. Each snapshot records the two chain tips it was built
//! against and the mode/completeness of its transaction set, so combined
//! ChainIndex + mempool reads can tell whether the mempool is coherent with the
//! caller's non-finalized snapshot.

use std::collections::HashMap;
use std::sync::Arc;

use zebra_chain::transaction::Hash as TxHash;

use crate::entry::MempoolEntry;
use crate::ports::{BlockRef, NonFinalizedEpoch};

/// The tip of the source that supplies mempool data ("V").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValidatorTip {
    /// The mempool source's best tip.
    pub best_tip: BlockRef,
}

/// The two tips the mempool tracks: the validator/mempool-source tip ("V") and
/// the non-finalized-state epoch ("NS").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservedTips {
    /// Latest observed validator/mempool-source tip.
    pub validator: Option<ValidatorTip>,
    /// Latest observed non-finalized-state epoch.
    pub non_finalized: Option<NonFinalizedEpoch>,
}

impl ObservedTips {
    /// The empty observation (neither tip known yet).
    pub fn none() -> Self {
        Self {
            validator: None,
            non_finalized: None,
        }
    }

    /// If both tips are known and their hashes agree, the agreed NS epoch the
    /// mempool set may mutate under. Otherwise `None`.
    pub fn agree(&self) -> Option<NonFinalizedEpoch> {
        let validator = self.validator?;
        let non_finalized = self.non_finalized?;

        if validator.best_tip.hash == non_finalized.best_tip.hash {
            Some(non_finalized)
        } else {
            None
        }
    }

    /// True when both tips are known but disagree.
    pub fn disagree(&self) -> bool {
        self.validator.is_some() && self.non_finalized.is_some() && self.agree().is_none()
    }
}

/// How a pair of observed tips changed between two ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TipChange {
    /// Neither tip changed.
    None,
    /// Only the validator tip changed.
    ValidatorChanged,
    /// Only the non-finalized tip changed.
    NonFinalizedChanged,
    /// Both tips changed.
    BothChanged,
}

/// Why the mempool transaction set is frozen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreezeReason {
    /// The non-finalized state is not available.
    NonFinalizedUnavailable,
    /// The mempool-source tip is not available.
    ValidatorTipUnavailable,
    /// The validator tip changed.
    ValidatorTipChanged,
    /// The non-finalized tip changed.
    NonFinalizedTipChanged,
    /// Both tips changed.
    BothTipsChanged,
    /// Both tips are known but disagree.
    TipsDiverged,
    /// A source error occurred; the last coherent set is retained.
    SourceError,
    /// A configured capacity bound was exceeded.
    CapacityLimited,
    /// The service is shutting down.
    Closing,
}

/// Whether the snapshot's transaction set is a complete view of the mempool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MempoolCompleteness {
    /// No coherent set has been built yet.
    NotReady,
    /// The set is a complete view of the source mempool at `valid_for`.
    Complete,
    /// The set is intentionally not complete because a capacity bound was hit.
    /// Full-mempool APIs must not present it as complete.
    IncompleteCapacityLimited,
    /// A source error occurred; the last coherent set may still be readable, but
    /// no live updates are being applied.
    IncompleteSourceError,
}

/// The published mode of the mempool snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MempoolMode {
    /// No coherent snapshot exists yet.
    NotReady,
    /// The transaction set is live and valid for `valid_for` (V == NS).
    Live {
        /// The agreed NS epoch the set is valid for.
        valid_for: NonFinalizedEpoch,
    },
    /// The transaction set is frozen at `valid_for` (or never had a coherent
    /// epoch), for `reason`.
    Frozen {
        /// The last agreed epoch the set is valid for, if any.
        valid_for: Option<NonFinalizedEpoch>,
        /// Why the set is frozen.
        reason: FreezeReason,
    },
    /// The service is closing.
    Closing,
}

/// An immutable snapshot of Zaino's mempool read model.
#[derive(Debug)]
pub struct MempoolSnapshot {
    /// The published mode of this snapshot.
    pub mode: MempoolMode,

    /// The last NS epoch this transaction set is valid for. `None` means there
    /// has never been a live coherent mempool.
    pub valid_for: Option<NonFinalizedEpoch>,

    /// The V and NS tips observed at publication.
    pub observed_tips: ObservedTips,

    /// Monotonic mempool generation (increments on each published set change).
    pub mempool_generation: u64,

    /// Monotonic event sequence (increments on each published snapshot).
    pub event_sequence: u64,

    /// Entries indexed by txid.
    pub by_txid: Arc<HashMap<TxHash, Arc<MempoolEntry>>>,

    /// Txids sorted by canonical byte order, for shortened-txid prefix lookup.
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
            mode: MempoolMode::NotReady,
            valid_for: None,
            observed_tips: ObservedTips::none(),
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

    /// True when this snapshot is a complete live set valid for exactly `epoch`.
    pub fn is_live_for(&self, epoch: NonFinalizedEpoch) -> bool {
        matches!(self.mode, MempoolMode::Live { valid_for } if valid_for == epoch)
            && self.valid_for == Some(epoch)
            && self.completeness == MempoolCompleteness::Complete
    }

    /// True when this snapshot's transaction set is valid for `epoch` (live or
    /// frozen at that epoch).
    pub fn is_valid_for_snapshot(&self, epoch: NonFinalizedEpoch) -> bool {
        self.valid_for == Some(epoch)
    }
}
