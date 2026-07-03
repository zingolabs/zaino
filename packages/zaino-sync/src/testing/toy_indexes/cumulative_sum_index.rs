//! SelfCumulative x Monoidal: running sum where extraction depends on
//! the accumulated state.
//!
//! Blocks whose prior running total exceeds a threshold contribute
//! double their value. This makes extraction genuinely dependent on
//! prior state — a BlockLocal index could not reproduce the same
//! result.

use crate::descriptor::{Monoidal, SelfCumulative};
use crate::encode::Encode;
use crate::primitives::IndexId;
use crate::traits::{
    ExtractCumulative, ExtractError, IndexDef, MergeMonoidal, Schema,
};

/// Block context for this index: just the block's value.
pub struct Context {
    /// Arbitrary value carried by this block.
    pub value: u32,
}

/// The accumulated sum — serves as both PriorState and Accumulator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CumulativeSum(u64);

impl CumulativeSum {
    /// Create a cumulative sum.
    pub const fn new(sum: u64) -> Self {
        Self(sum)
    }

    /// The raw numeric value.
    pub const fn value(&self) -> u64 {
        self.0
    }
}

impl Encode for CumulativeSum {
    fn encode(&self) -> Vec<u8> {
        self.0.to_le_bytes().to_vec()
    }
}

/// Cumulative sum where blocks past a threshold contribute double.
pub struct CumulativeSumIndex;

/// Index identity.
pub const ID: IndexId = IndexId::new("cumulative_sum");

/// Prior sums above this value cause blocks to contribute double.
const DOUBLING_THRESHOLD: u64 = 10;

impl IndexDef for CumulativeSumIndex {
    type Scope = SelfCumulative;
    type Composition = Monoidal;
    type Delta = u64;
    type BlockContext = Context;

    const NAME: IndexId = ID;
}

impl ExtractCumulative for CumulativeSumIndex {
    type PriorState = CumulativeSum;

    fn extract(ctx: &Context, prior: &CumulativeSum) -> Result<u64, ExtractError> {
        let base = u64::from(ctx.value);
        if prior.value() > DOUBLING_THRESHOLD {
            Ok(base * 2)
        } else {
            Ok(base)
        }
    }
}

impl MergeMonoidal for CumulativeSumIndex {
    type Accumulator = CumulativeSum;

    fn identity() -> CumulativeSum {
        CumulativeSum::new(0)
    }

    fn lift(delta: u64) -> CumulativeSum {
        CumulativeSum::new(delta)
    }

    fn combine(a: CumulativeSum, b: CumulativeSum) -> CumulativeSum {
        CumulativeSum::new(a.0 + b.0)
    }
}

impl Schema<CumulativeSum> for CumulativeSumIndex {
    type Key = &'static [u8];
    type Value = CumulativeSum;

    fn into_entries(sum: CumulativeSum) -> Vec<(Self::Key, Self::Value)> {
        vec![(b"sum".as_slice(), sum)]
    }
}
