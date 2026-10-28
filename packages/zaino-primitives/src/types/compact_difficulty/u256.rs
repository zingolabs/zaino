//! Minimal 256-bit unsigned arithmetic for the difficulty pipeline.
//!
//! Exactly the operations the nBits → target → work conversion needs — build a
//! target from its decoded mantissa and exponent, and divide `2^256` by
//! `target + 1` — implemented on a pair of `u128` halves. Nothing here leaves
//! the parent module: the expanded target is an implementation detail of
//! [`CompactDifficulty`](super::CompactDifficulty), not a domain quantity.

use core::num::NonZeroU128;

/// An unsigned 256-bit integer: `hi * 2^128 + lo`.
///
/// Field order is load-bearing: deriving `Ord` on `(hi, lo)` yields numeric
/// order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct U256 {
    hi: u128,
    lo: u128,
}

impl U256 {
    const ZERO: Self = Self { hi: 0, lo: 0 };

    /// The target `mantissa * 256^exponent`.
    ///
    /// The caller has already normalised the encoding: the mantissa occupies
    /// at most 24 bits and the exponent is at most 29, so the product spans at
    /// most `24 + 8·29 = 256` bits and the shift never discards a set bit.
    pub(super) fn target(mantissa: u32, exponent: u32) -> Self {
        Self {
            hi: 0,
            lo: u128::from(mantissa),
        }
        .shl(8 * exponent)
    }

    /// Whether this is zero.
    pub(super) fn is_zero(self) -> bool {
        self.hi == 0 && self.lo == 0
    }

    /// `floor(2^256 / (self + 1))`, when it fits 128 bits.
    ///
    /// `2^256` itself is not representable, so the quotient is computed as
    /// `((2^256 - self - 1) / (self + 1)) + 1`, whose numerator is exactly the
    /// bitwise complement of `self`. `None` when the result exceeds
    /// `u128::MAX` — a target below `2^128` — or on the unreachable wrap of
    /// `self + 1` (a target of all ones, which no compact encoding produces).
    pub(super) fn work(self) -> Option<NonZeroU128> {
        let divisor = self.checked_add_one()?;
        let quotient = self.complement().div(divisor);
        if quotient.hi != 0 {
            return None;
        }
        // The final `+ 1` of the quotient identity. `checked_add` refuses the
        // one remaining over-width value, `quotient + 1 == 2^128`, and makes
        // the result non-zero by construction.
        NonZeroU128::MIN.checked_add(quotient.lo)
    }

    /// The bitwise complement, i.e. `2^256 - 1 - self`.
    fn complement(self) -> Self {
        Self {
            hi: !self.hi,
            lo: !self.lo,
        }
    }

    /// `self + 1`, refusing to wrap.
    fn checked_add_one(self) -> Option<Self> {
        let (lo, carry) = self.lo.overflowing_add(1);
        let hi = if carry {
            self.hi.checked_add(1)?
        } else {
            self.hi
        };
        Some(Self { hi, lo })
    }

    /// `floor(self / divisor)` by restoring binary long division.
    ///
    /// Total for any non-zero divisor: the doubled remainder momentarily
    /// exceeds 256 bits only when the divisor does too, and that overflow is
    /// carried in a flag rather than lost — the subtraction that immediately
    /// follows brings the remainder back into range.
    fn div(self, divisor: Self) -> Self {
        let mut quotient = Self::ZERO;
        let mut remainder = Self::ZERO;
        for i in (0..256).rev() {
            // remainder < divisor here, so remainder·2 + bit − divisor fits
            // 256 bits even when the doubling itself carried out.
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

    /// Left shift, filling with zeroes; shifts of 256 or more yield zero.
    fn shl(self, n: u32) -> Self {
        match n {
            0 => self,
            1..=127 => Self {
                hi: (self.hi << n) | (self.lo >> (128 - n)),
                lo: self.lo << n,
            },
            128 => Self { hi: self.lo, lo: 0 },
            129..=255 => Self {
                hi: self.lo << (n - 128),
                lo: 0,
            },
            _ => Self::ZERO,
        }
    }

    /// The bit at position `i` (bit 0 is least significant).
    fn bit(self, i: u32) -> bool {
        if i < 128 {
            (self.lo >> i) & 1 == 1
        } else {
            (self.hi >> (i - 128)) & 1 == 1
        }
    }

    /// This value with the bit at position `i` set.
    fn set_bit(mut self, i: u32) -> Self {
        if i < 128 {
            self.lo |= 1 << i;
        } else {
            self.hi |= 1 << (i - 128);
        }
        self
    }

    /// `self - rhs` modulo `2^256`.
    ///
    /// Only the division uses this, on a remainder known (via the carry flag)
    /// to be at least the divisor, so the wrap is the reduction that brings a
    /// carried remainder back into 256 bits — never a silent underflow.
    fn wrapping_sub(self, rhs: Self) -> Self {
        let (lo, borrow) = self.lo.overflowing_sub(rhs.lo);
        let hi = self
            .hi
            .wrapping_sub(rhs.hi)
            .wrapping_sub(u128::from(borrow));
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
        assert_eq!(
            one.shl(127),
            U256 {
                hi: 0,
                lo: 1 << 127
            }
        );
        assert_eq!(one.shl(128), U256 { hi: 1, lo: 0 });
        assert_eq!(one.shl(129), U256 { hi: 2, lo: 0 });
        assert_eq!(
            one.shl(255),
            U256 {
                hi: 1 << 127,
                lo: 0
            }
        );
        assert_eq!(one.shl(256), U256::ZERO);
    }

    #[test]
    fn shl_carries_a_straddling_value() {
        // A value with bits in both halves after the shift.
        let x = from_u128(u128::MAX);
        assert_eq!(
            x.shl(1),
            U256 {
                hi: 1,
                lo: u128::MAX - 1
            }
        );
    }

    #[test]
    fn target_scales_by_powers_of_256() {
        assert_eq!(U256::target(0xffff, 0), from_u128(0xffff));
        assert_eq!(U256::target(0xffff, 1), from_u128(0xff_ff00));
        // 8·16 = 128 bits: lands exactly on the high half.
        assert_eq!(U256::target(1, 16), U256 { hi: 1, lo: 0 });
        // The largest shift the caller produces.
        assert_eq!(
            U256::target(0xff_0000, 29),
            U256 {
                hi: 0xff << 120,
                lo: 0
            }
        );
    }

    #[test]
    fn ord_is_numeric() {
        assert!(U256 { hi: 1, lo: 0 } > from_u128(u128::MAX));
        assert!(from_u128(2) > from_u128(1));
        assert!(
            U256 { hi: 2, lo: 0 }
                > U256 {
                    hi: 1,
                    lo: u128::MAX
                }
        );
    }

    #[test]
    fn checked_add_one_carries_and_refuses_the_wrap() {
        assert_eq!(
            from_u128(u128::MAX).checked_add_one(),
            Some(U256 { hi: 1, lo: 0 })
        );
        assert_eq!(
            U256 {
                hi: u128::MAX,
                lo: u128::MAX
            }
            .checked_add_one(),
            None
        );
    }

    #[test]
    fn div_small_values() {
        assert_eq!(from_u128(7).div(from_u128(2)), from_u128(3));
        assert_eq!(from_u128(6).div(from_u128(7)), U256::ZERO);
        assert_eq!(from_u128(6).div(from_u128(6)), from_u128(1));
    }

    #[test]
    fn div_across_the_half_boundary() {
        // (2^128 + 2) / 2 = 2^127 + 1.
        let dividend = U256 { hi: 1, lo: 2 };
        assert_eq!(dividend.div(from_u128(2)), from_u128((1 << 127) + 1));
        // (2^256 - 1) / (2^128 + 1) = 2^128 - 1.
        let all_ones = U256 {
            hi: u128::MAX,
            lo: u128::MAX,
        };
        assert_eq!(all_ones.div(U256 { hi: 1, lo: 1 }), from_u128(u128::MAX));
    }

    /// The doubled remainder can exceed 256 bits when the divisor does; the
    /// carry flag keeps the division exact there.
    #[test]
    fn div_by_a_divisor_above_two_to_the_255() {
        let dividend = U256 {
            hi: u128::MAX,
            lo: u128::MAX,
        };
        let divisor = U256 {
            hi: 1 << 127,
            lo: 1,
        };
        // floor((2^256 - 1) / (2^255 + 1)) = 1.
        assert_eq!(dividend.div(divisor), from_u128(1));
    }

    #[test]
    fn work_of_the_classic_minimum_difficulty_target() {
        // target = 0xffff · 2^208: floor(2^256 / (target+1)) = 0x1_0001_0001.
        let target = U256::target(0xffff, 26);
        assert_eq!(target.work(), NonZeroU128::new(0x1_0001_0001));
    }

    #[test]
    fn work_of_a_tiny_target_is_over_width() {
        // target = 1: work would be 2^255.
        assert_eq!(from_u128(1).work(), None);
    }

    #[test]
    fn work_fits_exactly_at_the_width_boundary() {
        // target = 2^128 - 1: work = 2^128, one past u128::MAX — refused.
        assert_eq!(from_u128(u128::MAX).work(), None);
        // target = 2^128: work = floor(2^256 / (2^128+1)) = 2^128 - 1 — fits.
        assert_eq!(U256 { hi: 1, lo: 0 }.work(), NonZeroU128::new(u128::MAX));
    }
}
