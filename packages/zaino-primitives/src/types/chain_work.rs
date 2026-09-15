//! Cumulative proof-of-work.

/// Cumulative chainwork at a block (256-bit big-endian).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainWork([u8; 32]);

impl ChainWork {
    /// No accumulated work: the value below genesis.
    pub const ZERO: Self = Self([0u8; 32]);

    /// Wrap raw chainwork bytes (256-bit big-endian).
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Add one block's 256-bit work to this cumulative total.
    ///
    /// Zcash defines block work as a 256-bit value. This performs exact
    /// 256-bit addition using two 128-bit limbs and returns `None` if the
    /// cumulative total would overflow 256 bits.
    pub fn checked_add(self, block_work: [u8; 32]) -> Option<Self> {
        let (high, low) = self.halves();
        let (work_high, work_low) = Self(block_work).halves();

        let (low, carried) = low.overflowing_add(work_low);

        let high = high
            .checked_add(work_high)?
            .checked_add(u128::from(carried))?;

        let mut bytes = [0u8; 32];
        bytes[..16].copy_from_slice(&high.to_be_bytes());
        bytes[16..].copy_from_slice(&low.to_be_bytes());

        Some(Self(bytes))
    }

    /// This total as its big-endian halves, most significant first.
    fn halves(self) -> (u128, u128) {
        let mut high = [0u8; 16];
        let mut low = [0u8; 16];

        high.copy_from_slice(&self.0[..16]);
        low.copy_from_slice(&self.0[16..]);

        (u128::from_be_bytes(high), u128::from_be_bytes(low))
    }
}

impl From<ChainWork> for [u8; 32] {
    fn from(cw: ChainWork) -> Self {
        cw.0
    }
}

impl From<[u8; 32]> for ChainWork {
    fn from(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::ChainWork;

    /// Encode a `u128` as a 256-bit big-endian work value.
    fn work_from_u128(work: u128) -> [u8; 32] {
        let mut bytes = [0u8; 32];
        bytes[16..].copy_from_slice(&work.to_be_bytes());
        bytes
    }

    /// A work value added to zero is that value.
    #[test]
    fn zero_is_the_identity() {
        let work = work_from_u128(42);

        let added = ChainWork::ZERO
            .checked_add(work)
            .expect("42 cannot overflow 256 bits");

        assert_eq!(added, ChainWork::new(work));
    }

    /// Adding is associative across a fold, which is what the freeze path and
    /// the chain-view rebase both rely on: accumulating block by block and
    /// accumulating from a running total must reach the same value.
    #[test]
    fn folding_block_work_matches_adding_the_sum() {
        let works = [7u128, 19, 1 << 100, 3];

        let folded = works.iter().try_fold(ChainWork::ZERO, |total, &work| {
            total.checked_add(work_from_u128(work))
        });

        let sum = works.iter().copied().sum::<u128>();
        let at_once = ChainWork::ZERO.checked_add(work_from_u128(sum));

        assert_eq!(folded, at_once);
    }

    /// A carry out of the low 128-bit limb lands in the high limb rather than
    /// wrapping the total back towards zero.
    #[test]
    fn a_carry_crosses_into_the_high_half() {
        let just_below = ChainWork::ZERO
            .checked_add(work_from_u128(u128::MAX))
            .expect("u128::MAX is below the 256-bit limit");

        let carried = just_below
            .checked_add(work_from_u128(1))
            .expect("2^128 is below the 256-bit limit");

        let mut expected = [0u8; 32];
        expected[15] = 1; // 2^128

        assert_eq!(carried, ChainWork::new(expected));
    }

    /// Per-block work may itself use the high 128 bits, as permitted by the
    /// Zcash work definition and represented by Zakura's 256-bit Work type.
    #[test]
    fn block_work_can_use_the_high_half() {
        let mut work = [0u8; 32];

        // high = 1, low = 42
        work[15] = 1;
        work[31] = 42;

        let added = ChainWork::ZERO
            .checked_add(work)
            .expect("this value is below the 256-bit limit");

        assert_eq!(added, ChainWork::new(work));
    }

    /// Overflow of the 256-bit cumulative total is refused, not wrapped.
    #[test]
    fn overflow_is_refused() {
        let at_the_top = ChainWork::new([0xff; 32]);

        assert_eq!(at_the_top.checked_add(work_from_u128(1)), None);
    }

    /// A total already at the maximum high 128-bit limb still admits work that
    /// does not cause a carry into that limb.
    #[test]
    fn a_full_high_half_still_accepts_work_that_does_not_carry() {
        let mut bytes = [0u8; 32];
        bytes[..16].fill(0xff);

        let result = ChainWork::new(bytes).checked_add(work_from_u128(1));

        assert!(result.is_some());
    }
}
