//! The total work of a chain up to a block.

use core::fmt;
use core::num::NonZeroU128;

/// The total work of a chain up to and including a block. Validators report
/// this value as `chainwork`.
///
/// `Ord`, because comparing total chain work is how the best chain is chosen.
/// That comparison is the only operation two of these values share; the
/// `arithmetic` module states the rest of the algebra.
///
/// Strictly positive. Every chain contains at least genesis, whose total work
/// is its own block work. A validator that does not track the value, or a block
/// with no parent, is `Option<AbsoluteChainWork>`. Absence is never a zero.
///
/// Recorded in 128 bits, against the 256 the wire carries. Real chains do not
/// approach either bound; [`try_from_reported`](Self::try_from_reported)
/// enforces the narrower one.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AbsoluteChainWork(NonZeroU128);

/// Error when reported chain work does not fit the recorded 128 bits.
///
/// Truncating would record less work than the chain has, which changes which
/// chain compares as heaviest. The bound fails loud instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("reported chainwork does not fit 128 bits (high half {high:#034x})")]
pub struct ChainWorkOverWidth {
    /// The non-zero high-order 128 bits of the rejected value.
    pub high: u128,
}

impl AbsoluteChainWork {
    /// Create a total chain work value.
    ///
    /// Infallible: the strictly-positive bound travels in the argument type.
    /// Bytes from a validator or from disk enter through
    /// [`try_from_reported`](Self::try_from_reported) instead.
    pub const fn new(value: NonZeroU128) -> Self {
        Self(value)
    }

    /// Read total chain work as a validator reports it: 32 big-endian bytes.
    ///
    /// Absorbs both conventions of the reporting surface, so no consumer
    /// repeats them:
    ///
    /// - **All-zero is `Ok(None)`.** Zero is not a possible amount of work, so
    ///   a validator that does not track the value (zebra hardcodes the field
    ///   to zero) is reporting absence, not a quantity to compare.
    /// - **The high 16 bytes must be zero**, or the value is refused.
    pub fn try_from_reported(bytes: [u8; 32]) -> Result<Option<Self>, ChainWorkOverWidth> {
        let (high, low) = split(bytes);
        if high != 0 {
            return Err(ChainWorkOverWidth { high });
        }
        Ok(NonZeroU128::new(low).map(Self))
    }

    /// Render as the 32 big-endian bytes the wire carries. Widening 128 bits to
    /// 256 cannot lose anything.
    pub fn to_be_bytes(&self) -> [u8; 32] {
        let mut bytes = [0u8; 32];
        bytes[16..].copy_from_slice(&self.0.get().to_be_bytes());
        bytes
    }

    /// The raw value, for the arithmetic relations to fold.
    pub(super) const fn into_raw(self) -> NonZeroU128 {
        self.0
    }
}

/// The two 128-bit halves of the 256-bit big-endian wire form.
fn split(bytes: [u8; 32]) -> (u128, u128) {
    let mut high = [0u8; 16];
    let mut low = [0u8; 16];
    high.copy_from_slice(&bytes[..16]);
    low.copy_from_slice(&bytes[16..]);
    (u128::from_be_bytes(high), u128::from_be_bytes(low))
}

impl From<AbsoluteChainWork> for NonZeroU128 {
    fn from(work: AbsoluteChainWork) -> Self {
        work.0
    }
}

impl fmt::Debug for AbsoluteChainWork {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("AbsoluteChainWork")
            .field(&format_args!("{:#x}", self.0))
            .finish()
    }
}

impl fmt::Display for AbsoluteChainWork {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#x}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn work(value: u128) -> AbsoluteChainWork {
        AbsoluteChainWork::new(NonZeroU128::new(value).expect("test value must be nonzero"))
    }

    /// All-zero off the wire is "not reported", not a smallest chain.
    #[test]
    fn reported_all_zero_is_absence() {
        assert_eq!(AbsoluteChainWork::try_from_reported([0u8; 32]), Ok(None));
    }

    /// A non-zero high half is refused, not truncated: a truncated value
    /// would be a *lower* cumulative work, which reorders chain selection.
    #[test]
    fn reported_over_width_is_refused() {
        let mut bytes = [0u8; 32];
        bytes[0] = 1;
        assert_eq!(
            AbsoluteChainWork::try_from_reported(bytes),
            Err(ChainWorkOverWidth { high: 1 << 120 })
        );
    }

    #[test]
    fn reported_bytes_round_trip() {
        let mut bytes = [0u8; 32];
        bytes[16..].copy_from_slice(&0x00de_ad00_beefu128.to_be_bytes());

        let reported = AbsoluteChainWork::try_from_reported(bytes)
            .expect("within width")
            .expect("non-zero");
        assert_eq!(reported.to_be_bytes(), bytes);
        assert_eq!(reported, work(0x00de_ad00_beef));
    }

    #[test]
    fn ord_selects_the_heavier_chain() {
        assert!(work(200) > work(100));
    }
}
