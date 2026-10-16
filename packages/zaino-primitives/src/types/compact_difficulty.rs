//! Header `nBits`: 8-bit base-256 exponent + 24-bit signed mantissa → 256-bit [`Target`]
//!
//! - work = `floor(2^256 / (target + 1))` (protocol [§7.7.4] `ToTarget`, [§7.7.5] work)
//! - Accepted (a validator's checks, before any hash compare): sign bit clear, target within 256
//!   bits, target non-zero, work within 128 bits
//! - Port of zebra-chain v6.4.2 `src/work/difficulty.rs` on `uint`'s U256 (= zebra's type)
//!
//! [§7.7.4]: https://zips.z.cash/protocol/protocol.pdf#nbits
//! [§7.7.5]: https://zips.z.cash/protocol/protocol.pdf#workdef

use core::fmt;
use core::num::NonZeroU64;

use zcash_encoding::ReverseHex;
use zcash_protocol::BlockHash;

// lints fire inside `uint`'s expansion (third-party code)
#[allow(clippy::manual_div_ceil, clippy::assign_op_pattern)]
mod wide {
    uint::construct_uint! {
        pub(super) struct U256(4);
    }
}

use wide::U256;

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

/// Threshold a header's hash must not exceed (zebra `ExpandedDifficulty`, ordered numerically)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Target(U256);

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

    pub fn bits(self) -> u32 {
        self.0
    }

    pub fn target(self) -> Target {
        expand(self.0).expect("validated at construction: target expands")
    }

    /// `2^256 / (target + 1)`
    pub fn work(self) -> u128 {
        self.target().work().expect("validated at construction: work fits")
    }
}

impl Target {
    /// `MeanTarget`: ⌊Σ / n⌋ as Σ quotients + ⌊Σ remainders / n⌋ (102 testnet targets overflow
    /// U256), `None` = no targets
    pub fn mean(targets: &[Self]) -> Option<Self> {
        let count = U256::from(targets.len());
        let (quotients, remainders) =
            targets.iter().fold((U256::zero(), U256::zero()), |(quotients, remainders), target| {
                let (quotient, remainder) = target.0.div_mod(count);
                (quotients + quotient, remainders + remainder)
            });
        (!targets.is_empty()).then(|| Self(quotients + remainders / count))
    }

    /// `self / expected * actual`, saturating (zcashd `CalculateNextWorkRequired`: divide first)
    pub fn rescale(self, actual: u64, expected: NonZeroU64) -> Self {
        Self((self.0 / U256::from(expected.get())).saturating_mul(U256::from(actual)))
    }

    /// `hash` read as a little-endian integer, at most the target
    pub fn is_met_by(self, hash: BlockHash) -> bool {
        U256::from_little_endian(&hash.0) <= self.0
    }

    /// `ExpandedDifficulty::to_compact` (bitcoin `GetCompact`): raw nBits, never negative
    ///
    /// - Unvalidated: a target below `2^128` encodes work past [`CompactDifficulty`]'s width
    pub fn to_compact(self) -> u32 {
        let size = self.0.bits() / 8 + 1;
        let mantissa = match self.0 <= U256::from(UNSIGNED_MANTISSA_MASK) {
            true => self.0 << (8 * (3 - size)),
            false => self.0 >> (8 * (size - 3)),
        };
        let size = u32::try_from(size).expect("a 256-bit target's byte length fits u32");
        mantissa.low_u32() + (size << PRECISION)
    }

    /// `floor(2^256 / (self + 1))` as `!self / (self + 1) + 1` (`None` past 128 bits)
    fn work(self) -> Option<u128> {
        let divisor = self.0.checked_add(U256::one())?;
        let work = (!self.0 / divisor).checked_add(U256::one())?;
        (work <= U256::from(u128::MAX)).then(|| work.as_u128())
    }
}

impl Target {
    /// Display-order (big-endian) hex, as zcashd `uint256S` (`chainparams.cpp` `powLimit`)
    pub fn from_hex(hex: &str) -> Option<Self> {
        ReverseHex::decode(hex).map(|bytes| Self(U256::from_little_endian(&bytes)))
    }
}

/// nBits → 256-bit target, validator check order: sign bit, exponent normalised (> 256 bits
/// refused), zero target
fn expand(bits: u32) -> Result<Target, CompactDifficultyError> {
    if bits & SIGN_BIT == SIGN_BIT {
        return Err(CompactDifficultyError::NegativeTarget { bits });
    }

    let mantissa = bits & UNSIGNED_MANTISSA_MASK;
    let raw_exponent = bits >> PRECISION;

    // - Normalised: mantissa <= 24 bits, exponent <= 29 → <= 256 bits (the shift drops nothing)
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

    let target = U256::from(mantissa) << (8 * exponent);
    if target.is_zero() {
        return Err(CompactDifficultyError::ZeroTarget { bits });
    }
    Ok(Target(target))
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

    /// Mainnet genesis (2^13), testnet limit, classic minimum (2^32 + 2^16 + 1), a mainnet block,
    /// regtest limit: each recompresses to itself, its work pinned
    #[test]
    fn known_nbits_recompress_to_themselves_and_pin_their_work() {
        for (bits, work) in [
            (0x1f07_ffff, 8_192),
            (0x2007_ffff, 32),
            (0x1d00_ffff, 0x1_0001_0001),
            (0x1c01_e1ab, 0x88_0f7f_03cf),
            (0x200f_0f0f, 17),
        ] {
            let difficulty = CompactDifficulty::try_from_bits(bits).expect("valid");
            let read = (difficulty.target().to_compact(), difficulty.work());
            assert_eq!(read, (bits, work), "{bits:#010x}");
        }
    }

    #[test]
    fn mean_floors_without_overflowing_256_bits() {
        let target = |bits| CompactDifficulty::try_from_bits(bits).expect("valid").target();
        let testnet_limit = target(0x2007_ffff);
        assert_eq!(Target::mean(&[testnet_limit; 102]), Some(testnet_limit), "Σ past 2^256");
        let small = [7u32, 8, 10].map(|n| Target(U256::from(n)));
        assert_eq!(Target::mean(&small), Some(Target(U256::from(8u32))), "⌊25 / 3⌋");
        assert_eq!(Target::mean(&[]), None);
    }

    /// 0x2007ffff = 0x07ffff · 256^29: bytes 29..=31 little-endian
    #[test]
    fn a_hash_meets_a_target_read_little_endian_up_to_equality() {
        let target = CompactDifficulty::try_from_bits(0x2007_ffff).expect("valid").target();
        let mut at_target = [0u8; 32];
        at_target[29..].copy_from_slice(&[0xff, 0xff, 0x07]);
        let mut above = at_target;
        above[0] = 1;
        assert_eq!([at_target, above].map(|hash| target.is_met_by(BlockHash(hash))), [true, false]);
    }

    #[test]
    fn displays_as_wire_hex() {
        let difficulty = CompactDifficulty::try_from_bits(0x1d00_ffff).expect("valid");
        assert_eq!(difficulty.to_string(), "1d00ffff");
        assert_eq!(format!("{difficulty:?}"), "CompactDifficulty(0x1d00ffff)");
    }
}
