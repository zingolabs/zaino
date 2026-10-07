//! 256-bit unsigned arithmetic for nBits → target → work, on two `u128` halves
//!
//! - Only what the conversion needs: target from mantissa + exponent, `2^256 / (target + 1)`
//! - Private to [`CompactDifficulty`](super::CompactDifficulty) (target = no domain quantity)

use core::num::NonZeroU128;

/// `hi * 2^128 + lo`; field order load-bearing (derived `Ord` on `(hi, lo)` = numeric order)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct U256 {
    hi: u128,
    lo: u128,
}

impl U256 {
    const ZERO: Self = Self { hi: 0, lo: 0 };

    /// `mantissa * 256^exponent`
    ///
    /// - Caller-normalised: mantissa <= 24 bits, exponent <= 29 → <= `24 + 8·29 = 256` bits (shift
    ///   never drops a set bit)
    pub(super) fn target(mantissa: u32, exponent: u32) -> Self {
        Self { hi: 0, lo: u128::from(mantissa) }.shl(8 * exponent)
    }

    pub(super) fn is_zero(self) -> bool {
        self.hi == 0 && self.lo == 0
    }

    /// `floor(2^256 / (self + 1))`, when it fits 128 bits
    ///
    /// - Computed as `((2^256 - self - 1) / (self + 1)) + 1` (`2^256` unrepresentable; numerator
    ///   = `!self`)
    /// - `None`: result > `u128::MAX` (target < `2^128`), or `self + 1` wraps (all ones: no
    ///   compact encoding produces it)
    pub(super) fn work(self) -> Option<NonZeroU128> {
        let divisor = self.checked_add_one()?;
        let quotient = self.complement().div(divisor);
        if quotient.hi != 0 {
            return None;
        }
        // Identity's `+ 1`: refuses `quotient + 1 == 2^128`, non-zero by construction
        NonZeroU128::MIN.checked_add(quotient.lo)
    }

    /// `2^256 - 1 - self`
    fn complement(self) -> Self {
        Self { hi: !self.hi, lo: !self.lo }
    }

    fn checked_add_one(self) -> Option<Self> {
        let (lo, carry) = self.lo.overflowing_add(1);
        let hi = if carry { self.hi.checked_add(1)? } else { self.hi };
        Some(Self { hi, lo })
    }

    /// `floor(self / divisor)`, restoring binary long division
    ///
    /// - Total for any non-zero divisor (doubled remainder past 256 bits only when the divisor is
    ///   too: carried in a flag, the next subtraction brings it back)
    fn div(self, divisor: Self) -> Self {
        let mut quotient = Self::ZERO;
        let mut remainder = Self::ZERO;
        for i in (0..256).rev() {
            // remainder < divisor → remainder·2 + bit − divisor fits 256 bits (even on carry)
            let carry = remainder.hi >> 127 != 0;
            remainder = remainder.shl(1);
            if self.bit(i) {
                remainder.lo |= 1;
            }
            if carry || remainder >= divisor {
                remainder = remainder.wrapping_sub(divisor);
                quotient = quotient.set_bit(i);
            }
        }
        quotient
    }

    /// Zero-filling; `n >= 256` → zero
    fn shl(self, n: u32) -> Self {
        match n {
            0 => self,
            1..=127 => Self { hi: (self.hi << n) | (self.lo >> (128 - n)), lo: self.lo << n },
            128 => Self { hi: self.lo, lo: 0 },
            129..=255 => Self { hi: self.lo << (n - 128), lo: 0 },
            _ => Self::ZERO,
        }
    }

    /// Bit 0 = least significant
    fn bit(self, i: u32) -> bool {
        if i < 128 {
            (self.lo >> i) & 1 == 1
        } else {
            (self.hi >> (i - 128)) & 1 == 1
        }
    }

    fn set_bit(mut self, i: u32) -> Self {
        if i < 128 {
            self.lo |= 1 << i;
        } else {
            self.hi |= 1 << (i - 128);
        }
        self
    }

    /// `self - rhs` mod `2^256`
    ///
    /// - `div` only, remainder >= divisor (carry flag): wrap = reduction back into 256 bits, never
    ///   a silent underflow
    fn wrapping_sub(self, rhs: Self) -> Self {
        let (lo, borrow) = self.lo.overflowing_sub(rhs.lo);
        let hi = self.hi.wrapping_sub(rhs.hi).wrapping_sub(u128::from(borrow));
        Self { hi, lo }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn from_u128(lo: u128) -> U256 {
        U256 { hi: 0, lo }
    }

    #[test]
    fn shl_moves_bits_across_the_half_boundary() {
        let one = from_u128(1);
        assert_eq!(one.shl(0), one);
        assert_eq!(one.shl(127), U256 { hi: 0, lo: 1 << 127 });
        assert_eq!(one.shl(128), U256 { hi: 1, lo: 0 });
        assert_eq!(one.shl(129), U256 { hi: 2, lo: 0 });
        assert_eq!(one.shl(255), U256 { hi: 1 << 127, lo: 0 });
        assert_eq!(one.shl(256), U256::ZERO);
    }

    #[test]
    fn shl_carries_a_straddling_value() {
        // bits in both halves after the shift
        let x = from_u128(u128::MAX);
        assert_eq!(x.shl(1), U256 { hi: 1, lo: u128::MAX - 1 });
    }

    #[test]
    fn target_scales_by_powers_of_256() {
        assert_eq!(U256::target(0xffff, 0), from_u128(0xffff));
        assert_eq!(U256::target(0xffff, 1), from_u128(0xff_ff00));
        // 8·16 = 128 bits: exactly the high half
        assert_eq!(U256::target(1, 16), U256 { hi: 1, lo: 0 });
        // largest shift the caller produces
        assert_eq!(U256::target(0xff_0000, 29), U256 { hi: 0xff << 120, lo: 0 });
    }

    #[test]
    fn ord_is_numeric() {
        assert!(U256 { hi: 1, lo: 0 } > from_u128(u128::MAX));
        assert!(from_u128(2) > from_u128(1));
        assert!(U256 { hi: 2, lo: 0 } > U256 { hi: 1, lo: u128::MAX });
    }

    #[test]
    fn checked_add_one_carries_and_refuses_the_wrap() {
        assert_eq!(from_u128(u128::MAX).checked_add_one(), Some(U256 { hi: 1, lo: 0 }));
        assert_eq!(U256 { hi: u128::MAX, lo: u128::MAX }.checked_add_one(), None);
    }

    #[test]
    fn div_small_values() {
        assert_eq!(from_u128(7).div(from_u128(2)), from_u128(3));
        assert_eq!(from_u128(6).div(from_u128(7)), U256::ZERO);
        assert_eq!(from_u128(6).div(from_u128(6)), from_u128(1));
    }

    #[test]
    fn div_across_the_half_boundary() {
        // (2^128 + 2) / 2 = 2^127 + 1
        let dividend = U256 { hi: 1, lo: 2 };
        assert_eq!(dividend.div(from_u128(2)), from_u128((1 << 127) + 1));
        // (2^256 - 1) / (2^128 + 1) = 2^128 - 1
        let all_ones = U256 { hi: u128::MAX, lo: u128::MAX };
        assert_eq!(all_ones.div(U256 { hi: 1, lo: 1 }), from_u128(u128::MAX));
    }

    /// Doubled remainder past 256 bits (divisor too): carry flag keeps it exact
    #[test]
    fn div_by_a_divisor_above_two_to_the_255() {
        let dividend = U256 { hi: u128::MAX, lo: u128::MAX };
        let divisor = U256 { hi: 1 << 127, lo: 1 };
        // floor((2^256 - 1) / (2^255 + 1)) = 1
        assert_eq!(dividend.div(divisor), from_u128(1));
    }

    #[test]
    fn work_of_the_classic_minimum_difficulty_target() {
        // target = 0xffff · 2^208: floor(2^256 / (target+1)) = 0x1_0001_0001
        let target = U256::target(0xffff, 26);
        assert_eq!(target.work(), NonZeroU128::new(0x1_0001_0001));
    }

    #[test]
    fn work_of_a_tiny_target_is_over_width() {
        // target = 1: work would be 2^255
        assert_eq!(from_u128(1).work(), None);
    }

    #[test]
    fn work_fits_exactly_at_the_width_boundary() {
        // target = 2^128 - 1: work = 2^128 = u128::MAX + 1 → refused
        assert_eq!(from_u128(u128::MAX).work(), None);
        // target = 2^128: work = floor(2^256 / (2^128+1)) = 2^128 - 1 → fits
        assert_eq!(U256 { hi: 1, lo: 0 }.work(), NonZeroU128::new(u128::MAX));
    }
}
