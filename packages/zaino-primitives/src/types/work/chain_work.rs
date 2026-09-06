//! The cumulative quantity: work accumulated along a chain.

use core::fmt;
use core::num::NonZeroU128;

use super::ZeroWork;

/// Cumulative proof-of-work at a block: the fold of block works along its
/// chain.
///
/// The ordering is the point. Chain selection compares cumulative work —
/// the heaviest chain wins — so this type derives [`Ord`]. There is
/// deliberately no `ChainWork + ChainWork`, because no chain is the
/// concatenation of two chains; the one relation between two cumulative
/// values, `since`, lands in [`RelativeWork`](super::RelativeWork), never
/// back in this type. Growing or shrinking a cumulative value takes a
/// [`BlockWork`](super::BlockWork), through the relations in the `arithmetic`
/// module.
///
/// Strictly positive: every chain contains at least genesis, whose cumulative
/// work is its own block work. Absence — a validator that does not track
/// cumulative work, a block with no parent — is `Option<ChainWork>`, never a
/// zero sentinel.
///
/// The RPC surface reports cumulative work as a 256-bit big-endian integer;
/// this type records 128 bits, which real chains do not approach. The width
/// bound is checked once, at the reported-bytes door, so a wider value fails
/// loud there instead of being truncated into a lower — and wrongly ordered —
/// cumulative work downstream.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChainWork(NonZeroU128);

/// Error when reported chainwork does not fit the recorded 128 bits.
///
/// Zcash's cumulative work is nowhere near `2^128`, so a value that does not
/// fit did not come from this chain. Truncating instead would record a lower
/// cumulative work than the chain actually has, which reorders chain
/// selection — so the width bound fails loud at the door.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("reported chainwork does not fit 128 bits (high half {high:#034x})")]
pub struct ChainWorkOverWidth {
    /// The non-zero high-order 128 bits of the rejected value.
    pub high: u128,
}

impl ChainWork {
    /// Create a cumulative work value, rejecting zero.
    ///
    /// The door for an already-narrowed integer — a value known to be
    /// cumulative work, such as one carried by another accumulator over the
    /// same chain. A value read off a validator's wire enters through
    /// [`try_from_reported`](Self::try_from_reported) instead, which also owns
    /// the width bound and the absence convention.
    pub fn try_new(value: u128) -> Result<Self, ZeroWork> {
        NonZeroU128::new(value).map(Self).ok_or(ZeroWork)
    }

    /// Read cumulative work as a validator reports it: 32 big-endian bytes.
    ///
    /// Two conventions of the reporting surface are absorbed here, so no
    /// consumer re-derives them:
    ///
    /// - **All-zero means not reported.** Zero is not a possible amount of
    ///   work for a real chain, so a validator that does not track cumulative
    ///   work (Zebra hardcodes the field to zero) is saying "no value", and
    ///   the answer is `Ok(None)` — absence as `Option`, never a zero
    ///   sentinel a consumer could mistakenly compare.
    /// - **The high 16 bytes must be zero.** The wire is 256 bits wide but
    ///   the quantity is recorded in 128; a wider value did not come from
    ///   this chain and is refused rather than truncated into a lower — and
    ///   wrongly ordered — cumulative work.
    pub fn try_from_reported(bytes: [u8; 32]) -> Result<Option<Self>, ChainWorkOverWidth> {
        let (high, low) = split(bytes);
        if high != 0 {
            return Err(ChainWorkOverWidth { high });
        }
        Ok(NonZeroU128::new(low).map(Self))
    }

    /// Render cumulative work as the 32 big-endian bytes the wire carries.
    ///
    /// Infallible widening: the recorded 128 bits left-pad into the 256-bit
    /// wire form without loss.
    pub fn to_be_bytes(&self) -> [u8; 32] {
        let mut bytes = [0u8; 32];
        bytes[16..].copy_from_slice(&self.0.get().to_be_bytes());
        bytes
    }

    /// Wrap the fold's running value.
    ///
    /// Module-internal: the arithmetic relations build cumulative work from
    /// the integer they fold into, and no unchecked external door exists.
    pub(super) const fn from_raw(raw: NonZeroU128) -> Self {
        Self(raw)
    }

    /// The raw accumulated value, for the arithmetic relations to fold.
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

impl From<ChainWork> for NonZeroU128 {
    fn from(work: ChainWork) -> Self {
        work.0
    }
}

impl fmt::Debug for ChainWork {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ChainWork")
            .field(&format_args!("{:#x}", self.0))
            .finish()
    }
}

impl fmt::Display for ChainWork {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#x}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_is_rejected_at_the_narrowed_door() {
        assert_eq!(ChainWork::try_new(0), Err(ZeroWork));
    }

    /// All-zero off the wire is "not reported", not a smallest chain.
    #[test]
    fn reported_all_zero_is_absence() {
        assert_eq!(ChainWork::try_from_reported([0u8; 32]), Ok(None));
    }

    /// A non-zero high half is refused, not truncated: a truncated value
    /// would be a *lower* cumulative work, which reorders chain selection.
    #[test]
    fn reported_over_width_is_refused() {
        let mut bytes = [0u8; 32];
        bytes[0] = 1;
        assert_eq!(
            ChainWork::try_from_reported(bytes),
            Err(ChainWorkOverWidth { high: 1 << 120 })
        );
    }

    #[test]
    fn reported_bytes_round_trip() {
        let mut bytes = [0u8; 32];
        bytes[16..].copy_from_slice(&0x00de_ad00_beefu128.to_be_bytes());

        let work = ChainWork::try_from_reported(bytes)
            .expect("within width")
            .expect("non-zero");
        assert_eq!(work.to_be_bytes(), bytes);
        assert_eq!(work, ChainWork::try_new(0x00de_ad00_beef).expect("nonzero"));
    }

    #[test]
    fn ord_selects_the_heavier_chain() {
        let lighter = ChainWork::try_new(100).expect("nonzero");
        let heavier = ChainWork::try_new(200).expect("nonzero");
        assert!(heavier > lighter);
    }
}
