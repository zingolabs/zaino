//! The lightwalletd-compatible serving use case.

use crate::controls::{Broadcast, MempoolContent, MempoolSubscribe, TakeSnapshot, TipSubscribe};
use crate::read_sets::LightWalletReads;

use super::{Serves, UseCase};

/// Lightwalletd-compatible serving: compact blocks to scan, treestate to
/// witness against, transparent history, raw transactions, broadcast and the
/// mempool. `GetLightdInfo` / `Ping` are serving metadata and belong to the
/// gRPC adapter, not this demand.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LightWallet;

impl UseCase for LightWallet {
    const NAME: &'static str = "light-wallet";
}

/// The light-wallet service: the read-set over a pin, plus broadcast and the
/// mempool.
pub trait LightWalletService:
    TakeSnapshot<Snapshot: LightWalletReads>
    + Broadcast
    + MempoolSubscribe
    + MempoolContent
    + TipSubscribe
{
}
impl<T> LightWalletService for T where
    T: TakeSnapshot<Snapshot: LightWalletReads>
        + Broadcast
        + MempoolSubscribe
        + MempoolContent
        + TipSubscribe
{
}

impl<S: LightWalletService> Serves<LightWallet> for S {}
