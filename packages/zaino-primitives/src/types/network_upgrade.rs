//! Network upgrade schedule as the validator reports it
//!
//! - No compiled-in schedule: adopted from the validator (indexer + validator never disagree)

use super::Height;

/// Protocol identity of a network upgrade (fixed by consensus: the stable key, unlike a name)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConsensusBranchId(u32);

impl ConsensusBranchId {
    pub const fn new(id: u32) -> Self {
        Self(id)
    }
}

impl From<ConsensusBranchId> for u32 {
    fn from(id: ConsensusBranchId) -> Self {
        id.0
    }
}

impl core::fmt::Display for ConsensusBranchId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // conventional form: 8-digit lowercase hex
        write!(f, "{:08x}", self.0)
    }
}

/// At the validator's current tip (zebrad never reports a disabled upgrade)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkUpgradeStatus {
    Active,
    Pending,
}

/// One schedule entry
///
/// - `branch_id` = the identity to key on
/// - `name`: display only, never matched (a validator ahead of Zaino names upgrades Zaino can't)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkUpgradeInfo {
    pub branch_id: ConsensusBranchId,
    pub name: String,
    pub activation_height: Height,
    pub status: NetworkUpgradeStatus,
}

/// Branch at the tip + for the next block (differ exactly when the next block activates one)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConsensusBranchIds {
    pub chain_tip: ConsensusBranchId,
    pub next_block: ConsensusBranchId,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_id_displays_as_eight_digit_hex() {
        assert_eq!(ConsensusBranchId::new(0xc2d6_d0b4).to_string(), "c2d6d0b4");
        assert_eq!(ConsensusBranchId::new(0).to_string(), "00000000");
    }
}
