//! Bounded coherent-stream events published by the tip-aware coherence layer.
//!
//! Gated behind `tip_aware_mempool`. Where the tip-agnostic core publishes plain
//! [`MempoolUpdate`](crate::update::MempoolUpdate) deltas, the coherence layer
//! republishes them as tip-keyed [`MempoolEvent`]s so the raw-transaction stream
//! can track the epoch it is serving: `Added` deltas carry `valid_for`, and the
//! stream closes on `Frozen`/`Closing` or when a new coherent epoch is reached.
//!
//! Events carry only the small facts the stream needs (the entry, the epoch, the
//! freeze reason) — never the whole `CoherentSnapshot`. Consumers that want the
//! full coherent view read it from
//! [`TipAwareMempool::coherent_snapshot`](crate::ports::TipAwareMempool::coherent_snapshot),
//! keeping buffered events tiny under many subscribers.

use std::sync::Arc;

use crate::entry::MempoolEntry;
use crate::ports::NonFinalizedEpoch;
use crate::tip::FreezeReason;

/// A coherent mempool change event, keyed to an NS epoch.
#[derive(Debug, Clone)]
pub enum MempoolEvent {
    /// A transaction was added to the coherent set. Never emitted while frozen.
    Added {
        /// Event sequence of the publishing coherent snapshot.
        sequence: u64,
        /// The epoch the coherent set is valid for.
        valid_for: NonFinalizedEpoch,
        /// The added entry (shared; not cloned per subscriber).
        entry: Arc<MempoolEntry>,
    },

    /// The coherent view was frozen. Live streams keep serving the last coherent
    /// set until the tips re-agree at a new epoch.
    Frozen {
        /// Event sequence of the frozen snapshot.
        sequence: u64,
        /// Why the view froze.
        reason: FreezeReason,
    },

    /// A fully reconciled live coherent snapshot was published for `valid_for`.
    Live {
        /// Event sequence of the live snapshot.
        sequence: u64,
        /// The epoch the coherent set is now live for.
        valid_for: NonFinalizedEpoch,
    },

    /// The service is closing.
    Closing {
        /// Event sequence of the closing event.
        sequence: u64,
    },
}
