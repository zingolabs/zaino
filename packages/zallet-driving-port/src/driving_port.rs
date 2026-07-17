//! The full live surface of the driving port.

use crate::broadcast_transaction::BroadcastTransaction;
use crate::get_health::GetHealth;
use crate::get_reported_upgrades::GetReportedUpgrades;
use crate::shut_down::ShutDown;
use crate::subscribe_to_mempool::SubscribeToMempool;
use crate::subscribe_to_tip_changes::SubscribeToTipChanges;
use crate::take_snapshot::TakeSnapshot;

/// The full live surface of the driving port: everything a driver
/// holds besides snapshots. One umbrella over the single-capability
/// traits, so consumers like Zallet bound one type parameter; the
/// pinned-read surface hangs off it through
/// [`TakeSnapshot::Snapshot`].
///
/// Blanket-implemented — implementations write the capability traits
/// and receive this for free.
pub trait DrivingPort:
    TakeSnapshot
    + SubscribeToTipChanges
    + SubscribeToMempool
    + BroadcastTransaction
    + GetReportedUpgrades
    + GetHealth
    + ShutDown
{
}

impl<T> DrivingPort for T where
    T: TakeSnapshot
        + SubscribeToTipChanges
        + SubscribeToMempool
        + BroadcastTransaction
        + GetReportedUpgrades
        + GetHealth
        + ShutDown
{
}
