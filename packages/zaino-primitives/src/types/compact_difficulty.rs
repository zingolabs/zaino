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
//! The whole bits → target → work pipeline is native to this crate: the domain
//! owns its arithmetic, and consensus implementations serve as differential
//! test oracles (the `zaino-convert-zebra` crate sweeps this module against
//! zebra's implementation across the encoding space) rather than as
//! dependencies. The expanded 256-bit target is deliberately internal — no
//! consumer reasons about targets, only about validity and work — so the
//! `u256` helper stays private to this module.
//!
//! [§7.7.4]: https://zips.z.cash/protocol/protocol.pdf#nbits
//! [§7.7.5]: https://zips.z.cash/protocol/protocol.pdf#workdef

mod u256;

use core::fmt;
use core::hash::{Hash, Hasher};

use super::work::SingleBlockWork;
use u256::U256;

/// Width of the mantissa field in bits, including its sign bit.
const PRECISION: u32 = 24;
/// The mantissa's sign bit. A set sign bit encodes a negative target.
const SIGN_BIT: u32 = 1 << (PRECISION - 1);
/// Mask selecting the mantissa's magnitude, and its maximum value.
const UNSIGNED_MANTISSA_MASK: u32 = SIGN_BIT - 1;
/// Exponent offset: a raw exponent of 3 leaves the mantissa unscaled.
const EXPONENT_OFFSET: u32 = 3;

/// A validated compact difficulty (`nBits`) value from a block header.
///
/// Invariant: `bits` expands to a valid target — non-negative, non-zero,
/// within 256 bits — whose work fits 128 bits, and `work` is that target's
/// work. `work` is a function of `bits`, so equality and hashing follow from
/// `bits` alone.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct CompactDifficulty {
    bits: u32,
    work: SingleBlockWork,
}

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
    /// Validate a raw `u32` nBits value.
    ///
    /// The boundary door for a value carried numerically — a parsed wire
    /// field, a stored row. Rejects every encoding outside the acceptance set,
    /// naming the broken rule.
    pub fn try_from_bits(bits: u32) -> Result<Self, CompactDifficultyError> {
        let work = expand(bits)?
            .work()
            .ok_or(CompactDifficultyError::WorkOverWidth { bits })?;
        Ok(Self {
            bits,
            work: SingleBlockWork::from(work),
        })
    }

    /// Validate nBits carried as its four big-endian (display-order) bytes.
    ///
    /// The same door as [`try_from_bits`](Self::try_from_bits) for call sites
    /// that already hold the display bytes — the byte order zebra's
    /// `bytes_in_display_order` and the hex wire forms use — sparing them a
    /// manual byte-order conversion.
    pub fn try_from_be_bytes(bytes: [u8; 4]) -> Result<Self, CompactDifficultyError> {
        Self::try_from_bits(u32::from_be_bytes(bytes))
    }

    /// The raw `u32` nBits value.
    ///
    /// For wire serialization and persistence; the value is guaranteed to be a
    /// valid compact encoding.
    pub fn as_bits(&self) -> u32 {
        self.bits
    }

    /// The proof-of-work this difficulty contributes to its chain:
    /// `floor(2^256 / (target + 1))`, per specification §7.7.5. Computed at
    /// construction.
    pub fn to_work(&self) -> SingleBlockWork {
        self.work
    }
}

impl Hash for CompactDifficulty {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.bits.hash(state);
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
        f.debug_tuple("CompactDifficulty")
            .field(&format_args!("{:#010x}", self.bits))
            .finish()
    }
}

impl fmt::Display for CompactDifficulty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The hex form the wire renders: eight lowercase digits, no prefix.
        write!(f, "{:08x}", self.bits)
    }
}

#[cfg(test)]
mod tests {
    use core::num::NonZeroU128;

    use super::*;

    /// A valid nBits value: the testnet/regtest proof-of-work limit.
    const TEST_VALID_NBITS: u32 = 0x2007_ffff;

    #[test]
    fn valid_bits_round_trip() {
        let cd = CompactDifficulty::try_from_bits(TEST_VALID_NBITS).expect("valid");
        assert_eq!(cd.as_bits(), TEST_VALID_NBITS);
    }

    #[test]
    fn be_bytes_door_matches_the_bits_door() {
        assert_eq!(
            CompactDifficulty::try_from_be_bytes([0x1d, 0x00, 0xff, 0xff]),
            CompactDifficulty::try_from_bits(0x1d00_ffff)
        );
    }

    #[test]
    fn zero_is_a_zero_target() {
        assert_eq!(
            CompactDifficulty::try_from_bits(0x0000_0000),
            Err(CompactDifficultyError::ZeroTarget { bits: 0 })
        );
    }

    #[test]
    fn sign_bit_is_a_negative_target() {
        assert_eq!(
            CompactDifficulty::try_from_bits(0x0180_0000),
            Err(CompactDifficultyError::NegativeTarget { bits: 0x0180_0000 })
        );
    }

    /// All ones has the sign bit set, so it rejects as negative before the
    /// oversized exponent is even looked at — the validator's check order.
    #[test]
    fn all_ones_rejects_as_negative() {
        assert_eq!(
            CompactDifficulty::try_from_bits(u32::MAX),
            Err(CompactDifficultyError::NegativeTarget { bits: u32::MAX })
        );
    }

    #[test]
    fn oversized_exponent_overflows() {
        assert_eq!(
            CompactDifficulty::try_from_bits(0x2300_0001),
            Err(CompactDifficultyError::OverflowTarget { bits: 0x2300_0001 })
        );
    }

    /// At the two boundary exponents, a mantissa wider than the room left
    /// overflows; one that fits is accepted.
    #[test]
    fn boundary_exponents_split_on_mantissa_width() {
        assert!(CompactDifficulty::try_from_bits(0x2200_00ff).is_ok());
        assert_eq!(
            CompactDifficulty::try_from_bits(0x2200_0100),
            Err(CompactDifficultyError::OverflowTarget { bits: 0x2200_0100 })
        );
        assert!(CompactDifficulty::try_from_bits(0x2100_ffff).is_ok());
        assert_eq!(
            CompactDifficulty::try_from_bits(0x2101_0000),
            Err(CompactDifficultyError::OverflowTarget { bits: 0x2101_0000 })
        );
    }

    /// A small exponent shifts the mantissa right; when nothing survives, the
    /// encoding is a zero target.
    #[test]
    fn underflow_to_zero_is_a_zero_target() {
        assert_eq!(
            CompactDifficulty::try_from_bits(0x0100_0100),
            Err(CompactDifficultyError::ZeroTarget { bits: 0x0100_0100 })
        );
    }

    fn work_of(bits: u32) -> u128 {
        let work = CompactDifficulty::try_from_bits(bits)
            .expect("valid")
            .to_work();
        NonZeroU128::from(work).get()
    }

    /// The Zcash mainnet proof-of-work limit (and genesis nBits):
    /// target `0x07ffff · 256^28`, work exactly `2^13`.
    #[test]
    fn work_of_the_mainnet_limit() {
        assert_eq!(work_of(0x1f07_ffff), 8192);
    }

    /// The testnet/regtest proof-of-work limit: target `0x07ffff · 256^29`,
    /// work exactly 32.
    #[test]
    fn work_of_the_testnet_limit() {
        assert_eq!(work_of(TEST_VALID_NBITS), 32);
    }

    /// The classic Bitcoin-family minimum-difficulty encoding:
    /// target `0xffff · 256^26`, work `0x1_0001_0001`.
    #[test]
    fn work_of_the_classic_minimum() {
        assert_eq!(work_of(0x1d00_ffff), 0x1_0001_0001);
    }

    /// A target of 1 is a well-formed encoding whose work (`2^255`) does not
    /// fit the 128-bit work width, so construction refuses it.
    #[test]
    fn tiny_target_work_is_over_width() {
        assert_eq!(
            CompactDifficulty::try_from_bits(0x0101_0000),
            Err(CompactDifficultyError::WorkOverWidth { bits: 0x0101_0000 })
        );
    }

    #[test]
    fn display_renders_the_wire_hex_form() {
        let cd = CompactDifficulty::try_from_bits(0x1d00_ffff).expect("valid");
        assert_eq!(cd.to_string(), "1d00ffff");
        assert_eq!(format!("{cd:?}"), "CompactDifficulty(0x1d00ffff)");
    }
}
