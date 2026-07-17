//! Consensus branch id of a network upgrade.

use core::fmt;

/// Consensus branch id (ZIP 200).
///
/// Identifies the consensus rules a transaction commits to. Every `u32` is
/// representable — branch ids are arbitrary constants assigned per network
/// upgrade — so construction is infallible. Display and RPC use 8-digit
/// lowercase hex (e.g. `76b809bb` for Sapling), matching
/// `getblockchaininfo`.
///
/// The inner value is private. Use `From<u32>` to construct and
/// `From<ConsensusBranchId> for u32` at boundaries that need the raw value.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConsensusBranchId(u32);

impl ConsensusBranchId {
    /// The Sprout branch id (`0x00000000`), in force before Overwinter.
    pub const SPROUT: Self = Self(0);
}

impl From<u32> for ConsensusBranchId {
    fn from(id: u32) -> Self {
        Self(id)
    }
}

impl From<ConsensusBranchId> for u32 {
    fn from(id: ConsensusBranchId) -> Self {
        id.0
    }
}

impl fmt::Debug for ConsensusBranchId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ConsensusBranchId({:08x})", self.0)
    }
}

impl fmt::Display for ConsensusBranchId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:08x}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sprout_is_zero() {
        assert_eq!(u32::from(ConsensusBranchId::SPROUT), 0);
    }

    #[test]
    fn roundtrip_u32() {
        let id = ConsensusBranchId::from(0x76b8_09bb);
        assert_eq!(u32::from(id), 0x76b8_09bb);
    }

    #[test]
    fn display_is_padded_lowercase_hex() {
        assert_eq!(
            format!("{}", ConsensusBranchId::from(0x76b8_09bb)),
            "76b809bb"
        );
        assert_eq!(format!("{}", ConsensusBranchId::SPROUT), "00000000");
    }

    #[test]
    fn debug_shows_hex() {
        let debug = format!("{:?}", ConsensusBranchId::from(0x76b8_09bb));
        assert_eq!(debug, "ConsensusBranchId(76b809bb)");
    }

    #[test]
    fn equality_and_ordering() {
        let a = ConsensusBranchId::from(1);
        let b = ConsensusBranchId::from(2);
        assert_ne!(a, b);
        assert!(a < b);
    }
}
