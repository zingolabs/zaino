//! The compact proof-of-work difficulty encoding from the block header.
//!
//! `nBits` is a custom floating-point format packed into a `u32`: an 8-bit
//! exponent over base 256 and a 24-bit signed mantissa. Expanding it gives the
//! 256-bit target threshold a block hash must fall under, and a block's work
//! is how much of the hash space that threshold excludes —
//! `floor(2^256 / (target + 1))`. Zcash protocol specification [§7.7.4]
//! (`ToTarget`) and [§7.7.5] (definition of work).
//!
//! Many `u32` bit patterns are not valid encodings. The acceptance set is what
//! a validator enforces before it ever compares a hash: the mantissa's sign
//! bit must be clear (a negative target is meaningless), the expanded value
//! must fit 256 bits (an oversized exponent, or a boundary exponent whose
//! mantissa is wider than the room left, overflows), and the expanded target
//! must be non-zero (a zero mantissa, or one shifted entirely away by a small
//! exponent, encodes no threshold). On top of those, the target's work must fit
//! the domain's 128-bit work width. [`CompactDifficulty`] is the proof that a
//! value passed those checks, and carries the work they computed.
//!
//! The whole bits → target → work pipeline is native to this crate, pinned by known mainnet /
//! testnet work vectors below. The expanded 256-bit target is deliberately internal — no
//! consumer reasons about targets, only about validity and work — so the
//! `u256` helper stays private to this module.
//!
//! [§7.7.4]: https://zips.z.cash/protocol/protocol.pdf#nbits
//! [§7.7.5]: https://zips.z.cash/protocol/protocol.pdf#workdef

mod u256;

use core::fmt;

use u256::U256;

/// Width of the mantissa field in bits, including its sign bit.
const PRECISION: u32 = 24;
/// The mantissa's sign bit. A set sign bit encodes a negative target.
const SIGN_BIT: u32 = 1 << (PRECISION - 1);
/// Mask selecting the mantissa's magnitude, and its maximum value.
const UNSIGNED_MANTISSA_MASK: u32 = SIGN_BIT - 1;
/// Exponent offset: a raw exponent of 3 leaves the mantissa unscaled.
const EXPONENT_OFFSET: u32 = 3;

/// Validated `nBits`: expands to a non-negative, non-zero target within 256 bits whose work fits
/// 128 bits
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct CompactDifficulty(u32);

/// Why a `u32` is not a valid compact difficulty encoding.
///
/// One variant per rejection in the acceptance set, so a boundary that refuses
/// a value can say which rule it broke.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CompactDifficultyError {
    /// The mantissa's sign bit is set, encoding a negative target.
    #[error("nBits {bits:#010x} encodes a negative target")]
    NegativeTarget {
        /// The rejected nBits value.
        bits: u32,
    },

    /// The expanded target is zero: the mantissa is zero, or a small exponent
    /// shifted it entirely away.
    #[error("nBits {bits:#010x} encodes a zero target")]
    ZeroTarget {
        /// The rejected nBits value.
        bits: u32,
    },

    /// The expanded target does not fit 256 bits.
    #[error("nBits {bits:#010x} encodes a target beyond 256 bits")]
    OverflowTarget {
        /// The rejected nBits value.
        bits: u32,
    },

    /// The target is below `2^128`, so its work does not fit 128 bits. No real
    /// chain reaches such difficulty; the value is refused rather than
    /// truncated into a lower, wrongly ordered work.
    #[error("nBits {bits:#010x} yields work exceeding 128 bits")]
    WorkOverWidth {
        /// The rejected nBits value.
        bits: u32,
    },
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
}

/// Decode nBits into its expanded 256-bit target, applying the acceptance set.
///
/// The checks and their order follow what validators do before comparing a
/// hash: reject the sign bit, normalise the exponent (rejecting values past
/// 256 bits), then reject a zero result.
fn expand(bits: u32) -> Result<U256, CompactDifficultyError> {
    if bits & SIGN_BIT == SIGN_BIT {
        return Err(CompactDifficultyError::NegativeTarget { bits });
    }

    let mantissa = bits & UNSIGNED_MANTISSA_MASK;
    let raw_exponent = bits >> PRECISION;

    // Normalise so the scaling cannot pass 256 bits on its own. At the two
    // boundary exponents the spare mantissa bytes absorb part of the shift —
    // an overflow whose overflowing bits are all zero is representable and
    // accepted, one with any bit set is rejected. A raw exponent below the
    // offset shifts the mantissa right instead.
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
        // The hex form the wire renders: eight lowercase digits, no prefix.
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
