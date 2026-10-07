//! Header `nBits`: 8-bit base-256 exponent + 24-bit signed mantissa → 256-bit target; work =
//! `floor(2^256 / (target + 1))` (protocol [§7.7.4] `ToTarget`, [§7.7.5] work)
//!
//! - Accepted (a validator's checks, before any hash compare): sign bit clear, target within 256
//!   bits, target non-zero, work within 128 bits
//! - Target stays private (`u256`): consumers reason about validity + work only
//!
//! [§7.7.4]: https://zips.z.cash/protocol/protocol.pdf#nbits
//! [§7.7.5]: https://zips.z.cash/protocol/protocol.pdf#workdef

mod u256;

use core::fmt;

use u256::U256;

/// Mantissa bits, sign bit included
const PRECISION: u32 = 24;
/// Set = negative target
const SIGN_BIT: u32 = 1 << (PRECISION - 1);
const UNSIGNED_MANTISSA_MASK: u32 = SIGN_BIT - 1;
/// Raw exponent 3 = mantissa unscaled
const EXPONENT_OFFSET: u32 = 3;

/// Validated `nBits`: expands to a non-negative, non-zero target within 256 bits whose work fits
/// 128 bits
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct CompactDifficulty(u32);

/// One variant per broken acceptance rule
///
/// - `ZeroTarget`: zero mantissa, or shifted away by a small exponent
/// - `WorkOverWidth`: target < `2^128` (refused, never truncated into a wrongly ordered work)
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CompactDifficultyError {
    #[error("nBits {bits:#010x} encodes a negative target")]
    NegativeTarget { bits: u32 },

    #[error("nBits {bits:#010x} encodes a zero target")]
    ZeroTarget { bits: u32 },

    #[error("nBits {bits:#010x} encodes a target beyond 256 bits")]
    OverflowTarget { bits: u32 },

    #[error("nBits {bits:#010x} yields work exceeding 128 bits")]
    WorkOverWidth { bits: u32 },
}

impl CompactDifficulty {
    /// Rejects every encoding outside the acceptance set, naming the broken rule
    pub fn try_from_bits(bits: u32) -> Result<Self, CompactDifficultyError> {
        expand(bits)?.work().ok_or(CompactDifficultyError::WorkOverWidth { bits })?;
        Ok(Self(bits))
    }

    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn bits(self) -> u32 {
        self.0
    }

    /// `2^256 / (target + 1)`
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn work(self) -> u128 {
        expand(self.0)
            .ok()
            .and_then(U256::work)
            .expect("validated at construction: work fits")
            .get()
    }
}

/// nBits → 256-bit target, validator check order: sign bit, exponent normalised (> 256 bits
/// refused), zero target
fn expand(bits: u32) -> Result<U256, CompactDifficultyError> {
    if bits & SIGN_BIT == SIGN_BIT {
        return Err(CompactDifficultyError::NegativeTarget { bits });
    }

    let mantissa = bits & UNSIGNED_MANTISSA_MASK;
    let raw_exponent = bits >> PRECISION;

    // - Normalised: scaling alone never passes 256 bits
    // - Two boundary exponents: spare mantissa bytes absorb the shift (overflowing bits all zero
    //   = accepted, any set = refused)
    // - Raw exponent < offset → mantissa shifted right
    let (mantissa, exponent) = if raw_exponent >= EXPONENT_OFFSET + 32 {
        return Err(CompactDifficultyError::OverflowTarget { bits });
    } else if raw_exponent == EXPONENT_OFFSET + 31 {
        if mantissa > u32::from(u8::MAX) {
            return Err(CompactDifficultyError::OverflowTarget { bits });
        }
        (mantissa << 16, 29)
    } else if raw_exponent == EXPONENT_OFFSET + 30 {
        if mantissa > u32::from(u16::MAX) {
            return Err(CompactDifficultyError::OverflowTarget { bits });
        }
        (mantissa << 8, 29)
    } else if raw_exponent < EXPONENT_OFFSET {
        (mantissa >> (8 * (EXPONENT_OFFSET - raw_exponent)), 0)
    } else {
        (mantissa, raw_exponent - EXPONENT_OFFSET)
    };

    let target = U256::target(mantissa, exponent);
    if target.is_zero() {
        return Err(CompactDifficultyError::ZeroTarget { bits });
    }
    Ok(target)
}

impl fmt::Debug for CompactDifficulty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CompactDifficulty").field(&format_args!("{:#010x}", self.0)).finish()
    }
}

impl fmt::Display for CompactDifficulty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // wire form: 8 lowercase hex digits, no prefix
        write!(f, "{:08x}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every rule in the acceptance set, in the validator's check order (all-ones rejects as
    /// negative before its oversized exponent is looked at)
    #[test]
    fn each_invalid_encoding_names_the_rule_it_breaks() {
        use CompactDifficultyError::*;
        for (bits, expected) in [
            (0x2007_ffff, None),
            (0x2200_00ff, None),
            (0x2100_ffff, None),
            (0x0000_0000, Some(ZeroTarget { bits: 0 })),
            (0x0100_0100, Some(ZeroTarget { bits: 0x0100_0100 })),
            (0x0180_0000, Some(NegativeTarget { bits: 0x0180_0000 })),
            (u32::MAX, Some(NegativeTarget { bits: u32::MAX })),
            (0x2300_0001, Some(OverflowTarget { bits: 0x2300_0001 })),
            (0x2200_0100, Some(OverflowTarget { bits: 0x2200_0100 })),
            (0x2101_0000, Some(OverflowTarget { bits: 0x2101_0000 })),
            (0x0101_0000, Some(WorkOverWidth { bits: 0x0101_0000 })),
        ] {
            assert_eq!(CompactDifficulty::try_from_bits(bits).err(), expected, "{bits:#010x}");
        }
    }

    /// Work pinned against known limits (mainnet genesis 2^13, testnet 32, classic minimum);
    /// rendered as the wire's 8-digit hex
    #[test]
    fn work_matches_known_limits_and_display_is_wire_hex() {
        for (bits, work) in [(0x1f07_ffff, 8192), (0x2007_ffff, 32), (0x1d00_ffff, 0x1_0001_0001)] {
            let computed = expand(bits).expect("valid").work().expect("fits").get();
            assert_eq!(computed, work, "{bits:#010x}");
        }
        let cd = CompactDifficulty::try_from_bits(0x1d00_ffff).expect("valid");
        assert_eq!(cd.to_string(), "1d00ffff");
        assert_eq!(format!("{cd:?}"), "CompactDifficulty(0x1d00ffff)");
    }
}
