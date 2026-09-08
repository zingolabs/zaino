//! Reported network-upgrade schedule, passed through from the validator.

use zaino_primitives::types::Height;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpgradeStatus {
    Active,
    Pending,
    Disabled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportedUpgrade {
    pub branch_id: u32,
    pub name: String,
    pub activation_height: Height,
    pub status: UpgradeStatus,
}
