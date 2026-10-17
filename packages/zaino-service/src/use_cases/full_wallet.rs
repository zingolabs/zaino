//! The full wallet embedded as a library.

use crate::controls::{Broadcast, MempoolSubscribe, ReportedUpgrades, TakeSnapshot, TipSubscribe};

use crate::read_sets::FullWalletReads;

use super::{Serves, UseCase};

/// A full wallet embedding Zaino as a library (zallet). Consumed in-process;
/// the wire-DTO stability layer lives in its adapter, not here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FullWallet;

impl UseCase for FullWallet {
    const NAME: &'static str = "full-wallet";
}

/// The full-wallet service: the read-set over a pin, plus broadcast, the
/// mempool and tip subscriptions, and the reported upgrade schedule.
pub trait FullWalletService:
    TakeSnapshot<Snapshot: FullWalletReads>
    + Broadcast
    + MempoolSubscribe
    + TipSubscribe
    + ReportedUpgrades
{
}
impl<T> FullWalletService for T where
    T: TakeSnapshot<Snapshot: FullWalletReads>
        + Broadcast
        + MempoolSubscribe
        + TipSubscribe
        + ReportedUpgrades
{
}

impl<S: FullWalletService> Serves<FullWallet> for S {}
