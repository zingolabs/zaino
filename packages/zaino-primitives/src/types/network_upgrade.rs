//! Network upgrade schedule as reported by the validator.
//!
//! Zaino does not carry a compiled-in activation schedule for the chain it
//! serves — it adopts the one the validator reports, so an indexer and its
//! validator cannot disagree about where an upgrade activates.

use super::Height;

/// A consensus branch identifier.
///
/// The protocol-defined identity of a network upgrade, and the stable key for
/// one: unlike a name, it is fixed by consensus and cannot be spelled two ways.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConsensusBranchId(u32);

impl ConsensusBranchId {
    /// Wrap a raw branch identifier.
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
        // Branch IDs are conventionally written as 8-digit lowercase hex.
        write!(f, "{:08x}", self.0)
    }
}

/// At the validator's current tip (zebrad never reports a disabled upgrade)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkUpgradeStatus {
    Active,
    /// Activation height not reached yet
    Pending,
}

/// One entry in the validator's upgrade schedule (`branch_id` = the identity to key on)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkUpgradeInfo {
    pub branch_id: ConsensusBranchId,
    /// Display only, never matched (a validator ahead of Zaino names upgrades Zaino can't)
    pub name: String,
    pub activation_height: Height,
    pub status: NetworkUpgradeStatus,
}

/// The consensus branches in force around the current tip.
///
/// The two differ exactly when the next block activates a network upgrade,
/// which is what makes this worth reporting as a pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConsensusBranchIds {
    /// Branch in force at the current tip.
    pub chain_tip: ConsensusBranchId,
    /// Branch that will be in force for the next block.
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
