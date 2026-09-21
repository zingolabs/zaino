//! The tip-agnostic mempool change feed.
//!
//! The core mempool publishes every set change as a [`MempoolUpdate`] over a
//! bounded broadcast channel (`Mempool::subscribe_updates`). This is the general
//! "what changed" feed the tip-aware coherence layer consumes to drive freeze/thaw
//! without re-fetching; it carries no chain-tip knowledge.
//!
//! # Consistency contract (read this before consuming the feed)
//!
//! The feed is delivered over a **bounded** channel, so it trades strict
//! per-delta delivery for bounded memory under thousands of consumers. It is
//! **lossless at the level of *state*, not every individual delta**. Two rules
//! make it safe:
//!
//! 1. **Subscribe before you read the current set.** Call `subscribe_updates`
//!    (or [`Mempool::subscribe_updates`](crate::ports::Mempool::subscribe_updates))
//!    *first*, then read `current()` — never the other way round — so no change
//!    slips through the gap between the two. Discard any buffered update whose
//!    `sequence` is at or below the snapshot you started from.
//! 2. **On [`MempoolUpdate::Lagged`], resync from `current()`.** A consumer that
//!    falls further behind than the channel's buffer is told so explicitly (it
//!    is *not* silently skipped): it must drop its incrementally-tracked state and
//!    re-read `current()`. [`MempoolUpdate::Reset`] is the same resync point after
//!    a normal republish. Because `current()` is always the authoritative latest
//!    set, a consumer never loses *state* — only intermediate deltas it can
//!    reconstruct from the fresh snapshot.
//!
//! The ergonomic `mempool_updates()` stream on the read handle folds the
//! transport's lag signal into an in-band [`MempoolUpdate::Lagged`] so this
//! contract is impossible to ignore; the raw `subscribe_updates` receiver surfaces
//! the same condition as `tokio::sync::broadcast::error::RecvError::Lagged`.

use std::sync::Arc;

use zaino_primitives::types::TransactionId;

use crate::entry::MempoolEntry;

/// A single change to the core mempool set.
#[derive(Debug, Clone)]
pub enum MempoolUpdate {
    /// A transaction entered the set.
    Added {
        /// Event sequence of the publishing snapshot.
        sequence: u64,
        /// The added entry (shared; not cloned per subscriber).
        entry: Arc<MempoolEntry>,
    },
    /// A transaction left the set.
    Removed {
        /// Event sequence of the publishing snapshot.
        sequence: u64,
        /// The removed transaction's id.
        txid: TransactionId,
    },
    /// A fully reconciled snapshot was published — the batch boundary after the
    /// per-transaction `Added`/`Removed` deltas for the same `sequence`. The set
    /// is now consistent; read `current()` for the coherent whole. (Carries only
    /// the sequence, never the snapshot itself, so buffered updates stay tiny.)
    Reset {
        /// Event sequence of the freshly published snapshot.
        sequence: u64,
    },
    /// The consumer fell behind the bounded feed and `missed` updates were
    /// coalesced away. **Resync from `current()`** — no *state* is lost, but the
    /// skipped deltas are gone.
    ///
    /// Not broadcast by the core: it is synthesized by the `mempool_updates()`
    /// stream from the transport's lag signal, so a consumer cannot silently miss
    /// a desync. Raw `subscribe_updates` consumers see the equivalent
    /// `broadcast::error::RecvError::Lagged`.
    Lagged {
        /// Number of updates skipped since the last delivered one.
        missed: u64,
    },
    /// The core is shutting down.
    Closing {
        /// Event sequence of the closing update.
        sequence: u64,
    },
}
