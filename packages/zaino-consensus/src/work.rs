//! Proof-of-work from a block header's compact difficulty.
//!
//! `nBits` is a floating-point encoding: an 8-bit exponent and a 24-bit signed
//! mantissa packed into a `u32`. Expanding it gives the target threshold a
//! block hash must fall under, and a block's work is how much of the hash space
//! that threshold excludes — `floor(2^256 / (target + 1))`.
//!
//! Zcash specification [§7.7.4] (`ToTarget`) and [§7.7.5] (definition of work).
//!
//! Many `u32` bit patterns are not valid encodings: negative mantissas, zero
//! targets, and exponents that would overflow 256 bits are all rejected rather
//! than clamped, matching what a validator does before it compares the hash.
//!
//! [§7.7.4]: https://zips.z.cash/protocol/protocol.pdf#nbits
//! [§7.7.5]: https://zips.z.cash/protocol/protocol.pdf#workdef

use primitive_types::U256;

/// Why an `nBits` value yields no work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WorkError {
    /// The value does not encode a valid target: the mantissa is negative, the
    /// target is zero, or the exponent overflows 256 bits.
    #[error("nBits {bits:#010x} does not encode a valid target")]
    InvalidTarget {
        /// The offending `nBits` value.
        bits: u32,
    },

    /// The target is valid but so small that its work exceeds 128 bits.
    ///
    /// Unreachable on a real chain — Zcash's total accumulated work is around
    /// 2^58 — but the encoding permits it, so it is rejected rather than
    /// truncated.
    #[error("nBits {bits:#010x} yields work exceeding 128 bits")]
    WorkOverflow {
        /// The offending `nBits` value.
        bits: u32,
    },
}

/// Exponent base: the mantissa is scaled by a power of 256.
const BASE: u32 = 256;
/// Exponent offset, so an exponent of 3 means "no scaling".
const OFFSET: i32 = 3;
/// Mantissa width in bits, including its sign bit.
const PRECISION: u32 = 24;
/// The mantissa's sign bit. A set sign bit means a negative target.
const SIGN_BIT: u32 = 1 << (PRECISION - 1);
/// Mask selecting the mantissa's magnitude, and its maximum value.
const UNSIGNED_MANTISSA_MASK: u32 = SIGN_BIT - 1;

/// Expand `nBits` into the 256-bit target threshold it encodes.
///
/// `None` for the encodings a validator rejects outright: negative mantissa,
/// zero target, or an exponent placing the value beyond 256 bits.
fn expand_target(bits: u32) -> Option<U256> {
    // A set sign bit means a negative target, which is rejected before the
    // hash is ever compared.
    if bits & SIGN_BIT == SIGN_BIT {
        return None;
    }

    let mantissa = bits & UNSIGNED_MANTISSA_MASK;
    // Safe: dividing a u32 by 2^24 leaves at most 8 bits.
    let exponent = i32::try_from(bits >> PRECISION).expect("fits in i32") - OFFSET;

    // Normalise before multiplying, so `BASE.pow(exponent)` cannot overflow
    // 256 bits on its own. An overflow whose overflowing bits are all zero is
    // representable and accepted; one with any bit set is rejected.
    // Underflows shift the mantissa right instead.
    let (mantissa, exponent) = match (mantissa, exponent) {
        // Beyond 256 bits regardless of mantissa.
        (_, e) if e >= 32 => return None,
        // At the boundary, a mantissa wider than the remaining bytes overflows.
        // Otherwise rescale: shifting the mantissa left by the bits the
        // exponent gives up leaves the product unchanged.
        (m, e) if e == 31 && m > u8::MAX.into() => return None,
        (m, e) if e == 31 => (m << 16, e - 2),
        (m, e) if e == 30 && m > u16::MAX.into() => return None,
        (m, e) if e == 30 => (m << 8, e - 1),
        // Underflow: the shift is by at most 3 bytes, since the offset is 3.
        (m, e) if e < 0 => (m >> ((e.abs() * 8) as u32), 0),
        (m, e) => (m, e),
    };

    let result = U256::from(mantissa) * U256::from(BASE).pow(U256::from(exponent));

    // A zero target is rejected without comparing the hash.
    (!result.is_zero()).then_some(result)
}

/// The proof-of-work contributed by a block with this `nBits`.
///
/// `floor(2^256 / (target + 1))`, per specification §7.7.5.
///
/// Returned as a `u128`, as the specification's value cannot be represented in
/// 256 bits for any real target and does not need to be: Zcash's total
/// accumulated work is around 2^58, and Bitcoin adds roughly 2^91 per year.
pub fn work_from_bits(bits: u32) -> Result<u128, WorkError> {
    let target = expand_target(bits).ok_or(WorkError::InvalidTarget { bits })?;

    // 2^256 is not representable in 256 bits, but 2^256 is at least
    // `target + 1`, so the quotient equals
    // `((2^256 - target - 1) / (target + 1)) + 1` — and `2^256 - target - 1`
    // is exactly the bitwise complement of the target.
    let work = (!target / (target + 1)) + 1;

    if work > U256::from(u128::MAX) {
        return Err(WorkError::WorkOverflow { bits });
    }
    Ok(work.as_u128())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A valid nBits value: non-negative, non-zero, no overflow.
    const VALID_NBITS: u32 = 0x2007_ffff;

    #[test]
    fn valid_bits_yield_work() {
        assert!(work_from_bits(VALID_NBITS).is_ok());
    }

    #[test]
    fn negative_mantissa_rejected() {
        assert_eq!(
            work_from_bits(0x0180_0000),
            Err(WorkError::InvalidTarget { bits: 0x0180_0000 })
        );
    }

    #[test]
    fn zero_target_rejected() {
        assert_eq!(
            work_from_bits(0x0000_0000),
            Err(WorkError::InvalidTarget { bits: 0 })
        );
    }

    #[test]
    fn overflowing_exponent_rejected() {
        assert_eq!(
            work_from_bits(u32::MAX),
            Err(WorkError::InvalidTarget { bits: u32::MAX })
        );
    }

    /// An easier target excludes less of the hash space, so it is worth less
    /// work. This ordering is the only property chain selection relies on.
    #[test]
    fn harder_targets_are_worth_more_work() {
        let easy = work_from_bits(0x2007_ffff).expect("valid");
        let hard = work_from_bits(0x1d00_ffff).expect("valid");
        assert!(hard > easy);
    }
}
