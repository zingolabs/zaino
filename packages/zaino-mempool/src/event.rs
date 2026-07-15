//! Bounded delta events published by the mempool service.
//!
//! Events flow over a bounded broadcast channel. Live streams consume `Added`
//! deltas and close on `Frozen`/`Closing`; `Live`/`Frozen` carry the freshly
//! published snapshot for subscribers that want the whole set.

use std::sync::Arc;

use zebra_chain::transaction::Hash as TxHash;

use crate::entry::MempoolEntry;
use crate::ports::NonFinalizedEpoch;
use crate::snapshot::{FreezeReason, MempoolSnapshot};

/// A mempool change event.
#[derive(Debug, Clone)]
pub enum MempoolEvent {
    /// A transaction was added to the live set. Never emitted while frozen.
    Added {
        /// Event sequence of the publishing snapshot.
        sequence: u64,
        /// The epoch the live set is valid for.
        valid_for: NonFinalizedEpoch,
        /// The added entry (shared; not cloned per subscriber).
        entry: Arc<MempoolEntry>,
    },

    /// A transaction was removed from the live set. Never emitted while frozen.
    Removed {
        /// Event sequence of the publishing snapshot.
        sequence: u64,
        /// The epoch the live set is valid for.
        valid_for: NonFinalizedEpoch,
        /// The removed transaction's id.
        txid: TxHash,
    },

    /// The transaction set was frozen. Live streams should close or resync.
    Frozen {
        /// Event sequence of the frozen snapshot.
        sequence: u64,
        /// The frozen snapshot.
        snapshot: Arc<MempoolSnapshot>,
        /// Why the set froze.
        reason: FreezeReason,
    },

    /// A fully reconciled live snapshot was published.
    Live {
        /// Event sequence of the live snapshot.
        sequence: u64,
        /// The live snapshot.
        snapshot: Arc<MempoolSnapshot>,
    },

    /// The service is closing.
    Closing {
        /// Event sequence of the closing event.
        sequence: u64,
    },
}
