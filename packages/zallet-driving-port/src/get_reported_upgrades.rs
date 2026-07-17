//! Capability: the network-upgrade schedule as the validator reports
//! it.

use std::future::Future;

use crate::error::PortError;
use crate::reported_upgrade::ReportedUpgrade;

/// Domain error for [`GetReportedUpgrades`].
///
/// Empty: the schedule always exists; only the backend can fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GetReportedUpgradesError {}

/// Report the network-upgrade schedule, ascending by activation
/// height.
///
/// Lives on the live port object, not the snapshot: the schedule is a
/// property of the network the engine follows, and each upgrade's
/// status reflects the current tip. The schedule is never empty —
/// every Zcash chain has at least one upgrade in force — so an
/// implementation that cannot obtain it fails with a backend error
/// rather than answering an empty list.
pub trait GetReportedUpgrades: Send + Sync {
    /// The reported upgrades, ascending by activation height.
    fn get_reported_upgrades(
        &self,
    ) -> impl Future<Output = Result<Vec<ReportedUpgrade>, PortError<GetReportedUpgradesError>>> + Send;
}
