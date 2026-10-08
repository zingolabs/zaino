//! The faults, one per violated link in the seam's inequality chain.

use thiserror::Error;
use zaino_primitives::types::Height;

/// A publish that would break the seam's invariant. A fault leaves the seam's
/// state unchanged, so the next legal publish is validated against the same
/// baseline as the rejected one.
///
/// There is no variant for `r <= t - d`: the seam owns the reorg depth and
/// derives `r` from the tip its caller supplies, so a horizon inside the reorg
/// window is unrepresentable rather than rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SeamFault {
    /// The volatile tier published a horizon below the one it already holds,
    /// which needs a reorg deeper than the consensus bound.
    #[error("horizon regressed to {to:?} from {held:?}")]
    RegressedHorizon {
        /// The rejected horizon.
        to: Height,
        /// The horizon still held.
        held: Height,
    },
    /// The durable tier published a watermark below the one it already holds.
    /// No rewind path exists, so this is corruption rather than a rollback.
    #[error("watermark regressed to {to:?} from {held:?}")]
    RegressedWatermark {
        /// The rejected watermark.
        to: Height,
        /// The watermark still held.
        held: Height,
    },
    /// The durable tier committed past the horizon, writing volatile heights
    /// into an append-only store.
    #[error("watermark {to:?} is past the horizon {horizon:?}")]
    WatermarkPastHorizon {
        /// The rejected watermark.
        to: Height,
        /// The horizon it exceeded.
        horizon: Height,
    },
}
