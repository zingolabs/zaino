//! A network upgrade as the validator reports it.

use zaino_primitives::types::{ConsensusBranchId, Height};

/// Whether a reported upgrade is in force.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpgradeStatus {
    /// The chain has reached the upgrade's activation height.
    Active,
    /// The activation height lies above the current tip.
    Pending,
}

/// One network upgrade as the validator reports it
/// (`getblockchaininfo`'s upgrade schedule, or the engine's
/// equivalent).
///
/// The port passes the validator's schedule through — activation
/// heights come from the validator, never from constants of the
/// port's own. Drivers feed this into their node-compatibility
/// checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportedUpgrade {
    /// The upgrade's consensus branch id.
    pub branch_id: ConsensusBranchId,
    /// The upgrade's human-readable name, as reported.
    pub name: String,
    /// The height the upgrade activates at.
    pub activation_height: Height,
    /// Whether the upgrade is in force at the current tip.
    pub status: UpgradeStatus,
}
