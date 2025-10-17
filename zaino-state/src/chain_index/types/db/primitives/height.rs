//! Block height type and utilities.

use core2::io::{self, Read, Write};

use crate::chain_index::encoding::{
    read_u32_be, version, write_u32_be, FixedEncodedLen, ZainoVersionedSerialise,
};

/// Block height.
///
/// NOTE: Encoded as 4-byte big-endian byte-string to ensure height ordering
/// for keys in Lexicographically sorted B-Tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct Height(pub(crate) u32);

/// The first block
pub const GENESIS_HEIGHT: Height = Height(0);

impl TryFrom<u32> for Height {
    type Error = &'static str;

    fn try_from(height: u32) -> Result<Self, Self::Error> {
        // Zebra enforces Height <= 2^31 - 1
        if height <= zebra_chain::block::Height::MAX.0 {
            Ok(Self(height))
        } else {
            Err("height must be ≤ 2^31 - 1")
        }
    }
}

impl From<Height> for u32 {
    fn from(h: Height) -> Self {
        h.0
    }
}

impl std::ops::Add<u32> for Height {
    type Output = Self;

    fn add(self, rhs: u32) -> Self::Output {
        Height(self.0 + rhs)
    }
}

impl std::ops::Sub<u32> for Height {
    type Output = Self;

    fn sub(self, rhs: u32) -> Self::Output {
        Height(self.0 - rhs)
    }
}

impl std::fmt::Display for Height {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::str::FromStr for Height {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let h = s.parse::<u32>().map_err(|_| "invalid u32")?;
        Self::try_from(h)
    }
}

impl From<Height> for zebra_chain::block::Height {
    fn from(h: Height) -> Self {
        zebra_chain::block::Height(h.0)
    }
}

impl TryFrom<zebra_chain::block::Height> for Height {
    type Error = &'static str;

    fn try_from(h: zebra_chain::block::Height) -> Result<Self, Self::Error> {
        Height::try_from(h.0)
    }
}

impl From<Height> for zcash_protocol::consensus::BlockHeight {
    fn from(h: Height) -> Self {
        zcash_protocol::consensus::BlockHeight::from(h.0)
    }
}

impl TryFrom<zcash_protocol::consensus::BlockHeight> for Height {
    type Error = &'static str;

    fn try_from(h: zcash_protocol::consensus::BlockHeight) -> Result<Self, Self::Error> {
        Height::try_from(u32::from(h))
    }
}

impl ZainoVersionedSerialise for Height {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        // Height must sort lexicographically - write **big-endian**
        write_u32_be(w, self.0)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let raw = read_u32_be(r)?;
        Height::try_from(raw).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

/// Height = 4-byte big-endian body.
impl FixedEncodedLen for Height {
    /// 4 bytes, BE
    const ENCODED_LEN: usize = 4;
}

#[cfg(test)]
mod height_safety_tests {
    use super::*;

    /// This test demonstrates the CRITICAL underflow bug in Height subtraction.
    /// When subtracting from genesis (Height(0)), the result wraps to u32::MAX
    /// in release mode, or panics in debug mode.
    ///
    /// Real-world impact: This breaks reorg detection at genesis in non_finalised_state.rs:390
    /// where the code does `let mut next_height_down = best_tip.height - 1;` without checking
    /// if height is 0. The wrapped value causes silent failure instead of ReorgError::AtGenesis.
    #[test]
    #[cfg(not(debug_assertions))] // Only runs in release mode
    fn test_underflow_wraps_to_max_in_release() {
        let genesis = GENESIS_HEIGHT; // Height(0)

        // In release mode, this wraps to u32::MAX instead of detecting the error
        let result = genesis - 1;

        // This assertion proves the bug: subtracting from 0 gives 4,294,967,295!
        assert_eq!(result.0, u32::MAX);
        assert_eq!(result.0, 4_294_967_295);
    }

    /// This test demonstrates the underflow bug panics in debug mode.
    /// Uncommenting this test in debug builds will cause a panic.
    #[test]
    #[should_panic]
    #[cfg(debug_assertions)] // Only runs in debug mode
    fn test_underflow_panics_in_debug() {
        let genesis = GENESIS_HEIGHT; // Height(0)

        // In debug mode, this panics with "attempt to subtract with overflow"
        let _result = genesis - 1;
    }

    /// This test demonstrates the overflow bug in Height addition.
    /// When adding to a height near u32::MAX, the result wraps around
    /// in release mode, or panics in debug mode.
    #[test]
    #[cfg(not(debug_assertions))] // Only runs in release mode
    fn test_overflow_wraps_in_release() {
        let high_height = Height(u32::MAX - 10);

        // Adding 20 to (u32::MAX - 10) should overflow
        // In release mode, this wraps around
        let result = high_height + 20;

        // Proves wraparound: (u32::MAX - 10) + 20 = 9
        assert_eq!(result.0, 9);

        // This could happen if:
        // 1. Corrupted database stores invalid height
        // 2. Arithmetic on heights near the limit
        // 3. Could cause incorrect block selection
    }

    /// This test demonstrates the overflow bug panics in debug mode.
    #[test]
    #[should_panic]
    #[cfg(debug_assertions)] // Only runs in debug mode
    fn test_overflow_panics_in_debug() {
        let high_height = Height(u32::MAX - 10);

        // In debug mode, this panics with "attempt to add with overflow"
        let _result = high_height + 20;
    }

    /// This test demonstrates field access allows bypassing validation.
    /// Height can only be constructed via TryFrom (validated), but arithmetic
    /// can produce invalid values.
    #[test]
    #[cfg(not(debug_assertions))]
    fn test_arithmetic_bypasses_validation() {
        // Construction is validated
        let result = Height::try_from(u32::MAX);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "height must be ≤ 2^31 - 1");

        // But arithmetic can create invalid heights!
        let valid_height = Height(zebra_chain::block::Height::MAX.0); // 2^31 - 1
        let invalid_height = valid_height + 1; // Now we have 2^31 (invalid!)

        // This height is > MAX but exists as a Height type
        assert!(invalid_height.0 > zebra_chain::block::Height::MAX.0);

        // Type system can't prevent this - the invariant is broken
    }
}
