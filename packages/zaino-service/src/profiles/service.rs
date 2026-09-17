//! Service profiles: a read-set over a pin, plus controls.
//!
//! Each composes a use case's read-set (via [`TakeSnapshot`]'s pin) with the
//! control capabilities that use case needs. Blanket-implemented, so the
//! runtime's single concrete engine satisfies every profile and each adapter
//! depends only on the one it needs.

use crate::controls::{
    Broadcast, MempoolSubscribe, Passthrough, ReportedUpgrades, TakeSnapshot, TipSubscribe,
};
use crate::profiles::read_sets::{FullWalletReads, LightWalletReads, NodeRpcReads};

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
