//! Tip-coherence types for the optional `tip_aware_mempool` layer.
//!
//! These types are gated behind the `tip_aware_mempool` feature. They describe
//! how the tip-agnostic core mempool set is made *coherent* with Zaino's
//! non-finalized-state (NS) tip: the two observed tips, whether they agree, and
//! the resulting [`CoherentSnapshot`] the coherence layer publishes.
//!
//! # Why a separate coherent view exists
//!
//! The core mempool set is always live and tip-*tagged* (it records the validator
//! tip `V` it was fetched at; see [`MempoolSnapshot::source_tip`]). Combined
//! ChainIndex reads (`get_raw_transaction`, `get_transaction_status`) and the raw
//! transaction stream must only serve the mempool when it is coherent with the
//! caller's NS snapshot — i.e. when `V == NS`. The coherence layer computes that
//! from the core's tagged set and the observed NS epoch, with **no re-fetch**: the
//! `source_tip` tag and the mempool data are a single-source pair, so `V == NS` is
//! sufficient to bless the set. This is the guarantee the whole rework rests on.

use std::sync::Arc;

use zaino_primitives::types::{BlockRef, TransactionId};

use crate::entry::MempoolEntry;
use crate::ports::NonFinalizedEpoch;
use crate::snapshot::MempoolSnapshot;

/// The two tips coherence tracks: the validator/mempool-source tip ("V", from the
/// core's [`source_tip`](MempoolSnapshot::source_tip) tag) and the
/// non-finalized-state epoch ("NS", from the [`NfsEpochObserver`](crate::ports::NfsEpochObserver)).
///
/// The V side is a plain [`BlockRef`]. The field name carries the role, and the
/// NS side is a distinct type, so the two cannot be confused at a call site —
/// a wrapper would add a name to unwrap rather than a mistake to prevent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservedTips {
    /// Latest observed validator/mempool-source tip ("V").
    pub validator: Option<BlockRef>,
    /// Latest observed non-finalized-state epoch ("NS").
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
    /// mempool set is coherent for. Otherwise `None`.
    pub fn agree(&self) -> Option<NonFinalizedEpoch> {
        let validator = self.validator?;
        let non_finalized = self.non_finalized?;

        if validator.hash == non_finalized.best_tip.hash {
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

/// How a pair of observed tips changed between two observations.
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

/// Why the coherent mempool view is frozen.
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
    /// The core set is incomplete (source error or capacity bound), so it cannot
    /// be blessed as a coherent view.
    CoreIncomplete,
    /// The service is shutting down.
    Closing,
}

/// The published mode of the coherent mempool view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MempoolMode {
    /// No coherent view exists yet.
    NotReady,
    /// The set is coherent and valid for `valid_for` (V == NS).
    Live {
        /// The agreed NS epoch the set is valid for.
        valid_for: NonFinalizedEpoch,
    },
    /// The set is frozen at `valid_for` (or never had a coherent epoch), for
    /// `reason`.
    Frozen {
        /// The last agreed epoch the set is valid for, if any.
        valid_for: Option<NonFinalizedEpoch>,
        /// Why the view is frozen.
        reason: FreezeReason,
    },
    /// The service is closing.
    Closing,
}

/// An immutable coherent view of the mempool, keyed to an NS epoch.
///
/// Wraps a tip-agnostic core [`MempoolSnapshot`] with the coherence metadata
/// (`mode`, `valid_for`, `observed_tips`). Combined ChainIndex reads consult this
/// so they only serve the mempool when it matches the caller's NS snapshot.
#[derive(Debug)]
pub struct CoherentSnapshot {
    /// The core mempool set this view wraps.
    pub set: Arc<MempoolSnapshot>,
    /// The published coherence mode.
    pub mode: MempoolMode,
    /// The NS epoch this set is coherent for. `None` means there has never been a
    /// live coherent mempool.
    pub valid_for: Option<NonFinalizedEpoch>,
    /// The V and NS tips observed at publication.
    pub observed_tips: ObservedTips,
    /// Monotonic coherent-event sequence.
    pub event_sequence: u64,
}

impl CoherentSnapshot {
    /// The initial, empty, not-ready coherent view.
    pub fn empty_not_ready() -> Self {
        Self {
            set: Arc::new(MempoolSnapshot::empty()),
            mode: MempoolMode::NotReady,
            valid_for: None,
            observed_tips: ObservedTips::none(),
            event_sequence: 0,
        }
    }

    /// True when this view's transaction set is valid for `epoch` (live or frozen
    /// at that epoch).
    pub fn is_valid_for_snapshot(&self, epoch: NonFinalizedEpoch) -> bool {
        self.valid_for == Some(epoch)
    }

    /// True when this view is a live coherent set valid for exactly `epoch`.
    pub fn is_live_for(&self, epoch: NonFinalizedEpoch) -> bool {
        matches!(self.mode, MempoolMode::Live { valid_for } if valid_for == epoch)
            && self.valid_for == Some(epoch)
    }

    /// The entry for `txid`, if present in the coherent set.
    pub fn get(&self, txid: &TransactionId) -> Option<Arc<MempoolEntry>> {
        self.set.by_txid().get(txid).cloned()
    }
}
