//! nBits ↔ 256-bit target, and work (port of zebra-chain v6.4.2 `src/work/difficulty.rs`)
//!
//! - target arithmetic on `uint`'s U256 (zebra's `primitive_types::U256` is the same type)

pub(crate) use wide::U256;

// lints fire inside `uint`'s expansion (third-party code)
#[allow(clippy::manual_div_ceil, clippy::assign_op_pattern)]
mod wide {
    uint::construct_uint! {
        pub(crate) struct U256(4);
    }
}

const OFFSET: i32 = 3;
const PRECISION: u32 = 24;
const SIGN_BIT: u32 = 1 << (PRECISION - 1);
const UNSIGNED_MANTISSA_MASK: u32 = SIGN_BIT - 1;

/// `CompactDifficulty::to_expanded`: `None` = negative, overflowing or zero (zcashd refuses each)
pub(crate) fn expand(bits: u32) -> Option<U256> {
    if bits & SIGN_BIT == SIGN_BIT {
        return None;
    }
    let mantissa = bits & UNSIGNED_MANTISSA_MASK;
    let exponent = i32::try_from(bits >> PRECISION).ok()? - OFFSET;
    let (mantissa, exponent) = match (mantissa, exponent) {
        (_, e) if e >= 32 => return None,
        (m, 31) if m > u32::from(u8::MAX) => return None,
        (m, 31) => (m << 16, 29),
        (m, 30) if m > u32::from(u16::MAX) => return None,
        (m, 30) => (m << 8, 29),
        (m, e) if e < 0 => (m.checked_shr(e.unsigned_abs() * 8).unwrap_or(0), 0),
        (m, e) => (m, e),
    };
    let target = U256::from(mantissa) * U256::from(256u32).pow(U256::from(exponent));
    (!target.is_zero()).then_some(target)
}

/// `ExpandedDifficulty::to_compact` (bitcoin `GetCompact`, never negative)
///
/// - `target` non-zero and ≤ 2^256 / 256 (every value the adjustment produces)
pub(crate) fn to_compact(target: U256) -> u32 {
    let size = target.bits() / 8 + 1;
    let mantissa = if target <= U256::from(UNSIGNED_MANTISSA_MASK) {
        target << (8 * (3 - size))
    } else {
        target >> (8 * (size - 3))
    };
    let size = u32::try_from(size).expect("a target's byte length fits u32");
    mantissa.low_u32() + (size << PRECISION)
}

/// `Work::try_from(ExpandedDifficulty)`: `2^256 / (target + 1)` as `!t / (t + 1) + 1`, `None` past
/// u128 (unreachable on any valid chain)
pub(crate) fn work(target: U256) -> Option<u128> {
    let work = (!target / (target + U256::one())) + U256::one();
    (work <= U256::from(u128::MAX)).then(|| work.as_u128())
}

/// `MeanTarget`: ⌊Σ / n⌋ as Σ quotients + ⌊Σ remainders / n⌋ (102 testnet targets overflow U256)
pub(crate) fn mean(targets: &[U256]) -> U256 {
    let count = U256::from(targets.len());
    let (quotients, remainders) =
        targets.iter().fold((U256::zero(), U256::zero()), |(quotients, remainders), target| {
            let (quotient, remainder) = target.div_mod(count);
            (quotients + quotient, remainders + remainder)
        });
    quotients + remainders / count
}

/// `ExpandedDifficulty::from_hash`: the hash as a little-endian integer, ≤ target to pass
pub(crate) fn meets(hash: [u8; 32], target: U256) -> bool {
    U256::from_little_endian(&hash) <= target
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Known encodings expand and recompress to themselves; zcashd's refusals stay refused; work
    /// of mainnet's genesis bits = 2^13 (`CompactDifficulty`'s own pin, zaino-primitives)
    #[test]
    fn bits_round_trip_and_work_matches_known_values() {
        for bits in [0x1f07_ffff, 0x2007_ffff, 0x1d00_ffff, 0x1c01_e1ab, 0x200f_0f0f] {
            let target = expand(bits).expect("valid");
            assert_eq!(to_compact(target), bits, "{bits:#010x}");
        }
        for invalid in [0x0000_0000, 0x0180_0000, 0x2300_0001, 0x2200_0100, 0x2101_0000] {
            assert_eq!(expand(invalid), None, "{invalid:#010x}");
        }
        let genesis = expand(0x1f07_ffff).expect("valid");
        assert_eq!(work(genesis), Some(8192));
        assert_eq!(work(expand(0x1d00_ffff).expect("valid")), Some(0x1_0001_0001));
        assert!(meets([0; 32], genesis) && !meets([0xff; 32], genesis));

        let testnet_limit = expand(0x2007_ffff).expect("valid");
        assert_eq!(mean(&[testnet_limit; 102]), testnet_limit, "Σ of 102 overflows 256 bits");
        let odd = [U256::from(7u32), U256::from(8u32), U256::from(10u32)];
        assert_eq!(mean(&odd), U256::from(8u32), "⌊25 / 3⌋ (remainders 1 + 2 + 1 carry)");
    }
}
