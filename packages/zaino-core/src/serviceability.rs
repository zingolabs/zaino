//! Which capabilities are answerable now, and a snapshot's serviceable range.

use zaino_primitives::types::Height;

/// The capability axis — one variant per capability trait, mirroring the index
/// set that backs it. Serviceability is "is this capability's index built?".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Capability {
    Blocks,
    Transactions,
    Treestate,
    AddressHistory,
    SpendStatus,
    SubtreeRoots,
    Mempool,
    Broadcast,
    ReportedUpgrades,
}

/// For each capability, the height it is answerable up to (`None` = not yet).
/// Derived from per-index sync progress (derivation lands here later).
#[derive(Clone, Debug, Default)]
pub struct ServiceabilityManifest {
    pub answerable: Vec<(Capability, Option<Height>)>,
}

/// The heights a snapshot can answer, and the FS/NFS boundary within them.
#[derive(Clone, Copy, Debug)]
pub struct ServiceableRange {
    /// Top of append-only finalised state.
    pub finalized_tip: Height,
    /// Pinned best-chain tip; `finalized_tip..=tip` is the non-finalised window.
    pub tip: Height,
}
