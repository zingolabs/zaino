//! Recent facet: spend status of a transparent outpoint within the window.

use zaino_core::{Outpoint, SpendStatus};

/// Whether an outpoint was spent within the reorg window. Re-derived from the
/// window's blocks; infallible. Merged with the FS spend index by the runtime.
pub trait NfsSpendFacts: Send + Sync {
    fn spend_status(&self, outpoint: Outpoint) -> SpendStatus;
}
