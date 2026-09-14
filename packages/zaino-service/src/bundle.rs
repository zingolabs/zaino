//! Bundle super-traits: the single aggregate handles the runtime declares it
//! implements, and the object-safe-ish seams if `dyn` is ever needed. Fine
//! traits are for consumers/mocks; these are for "the whole thing".

use zaino_core::ServiceableRange;

use crate::controls::{
    Broadcast, MempoolSubscribe, ReportedUpgrades, Serviceable, TakeSnapshot, TipSubscribe,
};
use crate::reads::{
    AddressRead, BlockRead, CompactBlockRead, ForkReconcile, SpendRead, TransactionRead,
    TreestateRead,
};

/// A pinned, reorg-coherent view. Every read observes the chain as of the tip
/// it was pinned to, for as long as any clone lives (ADR-0003).
pub trait Snapshot:
    BlockRead
    + CompactBlockRead
    + TransactionRead
    + TreestateRead
    + AddressRead
    + SpendRead
    + ForkReconcile
    + Clone
    + Send
    + Sync
    + 'static
{
    fn serviceable_range(&self) -> ServiceableRange;
}

/// The full inner driving surface — what `zaino-runtime` implements and what a
/// mock stands in for when testing outer clients.
pub trait IndexerService:
    TakeSnapshot
    + TipSubscribe
    + MempoolSubscribe
    + Broadcast
    + Serviceable
    + ReportedUpgrades
    + Send
    + Sync
    + 'static
{
}
