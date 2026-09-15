//! Consumer-facing capability bundles.
//!
//! Two altitudes, both derived from the public use cases (full-wallet library,
//! light-wallet serving, node-RPC/explorer serving):
//!
//! - **Read-sets** name *the demand* — which reads a use case pulls through a
//!   pinned view. They are the `required` capability set of the availability
//!   model, made first-class. Coherence is orthogonal and asserted once, by
//!   [`TakeSnapshot`]'s associated-type bound (`type Snapshot: Snapshot`).
//! - **Service profiles** compose a use case's read-set (via the pin) with the
//!   control capabilities it needs.
//!
//! Both altitudes are blanket-implemented: a type *is* a bundle exactly when it
//! has the constituent capabilities — the demand-first, structural rule the
//! source adapters also follow. So the runtime's single concrete engine
//! satisfies every profile, and each adapter depends only on the one it needs.

use crate::controls::{
    Broadcast, MempoolSubscribe, Passthrough, ReportedUpgrades, TakeSnapshot, TipSubscribe,
};
use crate::reads::{
    AddressRead, BlockRead, ChainInfoRead, CompactBlockRead, CompactNullifierRead, SpendRead,
    TransactionRead, TreestateRead,
};

// --- read-sets: the demand, named --------------------------------------------

/// Reads shared by every wallet-shaped consumer — scan compact blocks, build
/// note-commitment witnesses, track transparent funds. The common base of
/// [`FullWalletReads`] and [`LightWalletReads`], which are siblings over it: extracting
/// the core keeps the two from evolving through each other.
pub trait WalletReadCore: CompactBlockRead + TreestateRead + AddressRead + TransactionRead {}
impl<T> WalletReadCore for T where
    T: CompactBlockRead + TreestateRead + AddressRead + TransactionRead
{
}

/// The full-wallet library's read demand. A sibling of [`LightWalletReads`] over
/// [`WalletReadCore`] — full-wallet-only reads land here as its own delta, never
/// on a line the light path shares.
pub trait FullWalletReads: WalletReadCore {}
impl<T> FullWalletReads for T where T: WalletReadCore {}

/// The lightwalletd-compatible read demand. A sibling of [`FullWalletReads`] over
/// [`WalletReadCore`]; its delta is the compact-block nullifier serving variant.
pub trait LightWalletReads: WalletReadCore + CompactNullifierRead {}
impl<T> LightWalletReads for T where T: WalletReadCore + CompactNullifierRead {}

/// The node-RPC / explorer read demand: raw blocks and spend lookups the
/// wallet-shaped consumers never need, plus the chain-info aggregate. A distinct
/// shape, not a wallet delta.
pub trait NodeRpcReads:
    BlockRead + TransactionRead + SpendRead + AddressRead + TreestateRead + ChainInfoRead
{
}
impl<T> NodeRpcReads for T where
    T: BlockRead + TransactionRead + SpendRead + AddressRead + TreestateRead + ChainInfoRead
{
}

// --- service profiles: a read-set over a pin, plus controls ------------------

/// Full wallet embedded as a library. Consumed in-process; the wire-DTO
/// stability layer lives in its adapter, not here.
pub trait WalletLibService:
    TakeSnapshot<Snapshot: FullWalletReads>
    + Broadcast
    + MempoolSubscribe
    + TipSubscribe
    + ReportedUpgrades
{
}
impl<T> WalletLibService for T where
    T: TakeSnapshot<Snapshot: FullWalletReads>
        + Broadcast
        + MempoolSubscribe
        + TipSubscribe
        + ReportedUpgrades
{
}

/// Lightwalletd-compatible serving. `GetLightdInfo` / `Ping` are serving
/// metadata and belong to the gRPC adapter, not this port.
pub trait LightServeService:
    TakeSnapshot<Snapshot: LightWalletReads> + Broadcast + MempoolSubscribe + TipSubscribe
{
}
impl<T> LightServeService for T where
    T: TakeSnapshot<Snapshot: LightWalletReads> + Broadcast + MempoolSubscribe + TipSubscribe
{
}

/// Node-RPC / explorer serving. Composes the node read-set (incl. the chain-info
/// aggregate) with the validator passthrough seam (mining/peers/txoutset).
pub trait NodeRpcService:
    TakeSnapshot<Snapshot: NodeRpcReads> + Broadcast + MempoolSubscribe + TipSubscribe + Passthrough
{
}
impl<T> NodeRpcService for T where
    T: TakeSnapshot<Snapshot: NodeRpcReads>
        + Broadcast
        + MempoolSubscribe
        + TipSubscribe
        + Passthrough
{
}
