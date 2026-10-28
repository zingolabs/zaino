//! `getchaintips` — the known tips of the block tree.

use crate::types::{BlockHash, Height};

/// One known tip of the block tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainTip {
    /// Height of this tip.
    pub height: Height,
    /// Block hash of this tip.
    pub hash: BlockHash,
    /// Length of the branch connecting this tip to the active chain.
    ///
    /// Zero for the active tip itself.
    pub branch_len: u32,
    /// How far this branch has been validated.
    pub status: ChainTipStatus,
}

/// Validation status of a chain tip, as defined by the Zcash JSON-RPC
/// interface.
///
/// Ordered from least to most validated. A validator that reports a status
/// outside this set is reporting something the Zcash RPC interface does not
/// define; the adapter maps it to [`Self::Unknown`] rather than failing, so an
/// unfamiliar validator cannot break tip enumeration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainTipStatus {
    /// The branch contains at least one invalid block.
    Invalid,
    /// Headers are valid, but not all blocks for the branch are available.
    HeadersOnly,
    /// All headers are valid and available; blocks were never fully validated.
    ValidHeaders,
    /// Fully validated, but not part of the active chain.
    ValidFork,
    /// The tip of the active best chain.
    Active,
    /// The validator did not report a status this interface defines.
    Unknown,
}

impl ChainTipStatus {
    /// Whether this tip is the active best-chain tip.
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Active)
    }
}

impl core::fmt::Display for ChainTipStatus {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let name = match self {
            Self::Invalid => "invalid",
            Self::HeadersOnly => "headers-only",
            Self::ValidHeaders => "valid-headers",
            Self::ValidFork => "valid-fork",
            Self::Active => "active",
            Self::Unknown => "unknown",
        };
        f.write_str(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kebab-case spellings are the wire vocabulary of the Zcash RPC
    /// interface, not free-form labels — adapters serialise straight from
    /// these, so a rename is a protocol change.
    #[test]
    fn status_names_match_the_rpc_vocabulary() {
        assert_eq!(ChainTipStatus::HeadersOnly.to_string(), "headers-only");
        assert_eq!(ChainTipStatus::ValidHeaders.to_string(), "valid-headers");
        assert_eq!(ChainTipStatus::ValidFork.to_string(), "valid-fork");
        assert_eq!(ChainTipStatus::Active.to_string(), "active");
    }

    #[test]
    fn only_active_is_active() {
        assert!(ChainTipStatus::Active.is_active());
        assert!(!ChainTipStatus::ValidFork.is_active());
        assert!(!ChainTipStatus::Unknown.is_active());
    }
}
