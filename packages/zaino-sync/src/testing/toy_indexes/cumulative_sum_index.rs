//! SelfCumulative x Monoidal: running sum where extraction depends on
//! the accumulated state.
//!
//! Blocks whose prior running total exceeds a threshold contribute
//! double their value. This makes extraction genuinely dependent on
//! prior state — a BlockLocal index could not reproduce the same
//! result.

use crate::descriptor::{Monoidal, SelfCumulative};
use crate::primitives::IndexId;
use crate::traits::{ExtractCumulative, IndexDef, MergeMonoidal, Schema};
use zaino_persistence_codec::{DecodeError as PersistDecodeError, EntryCodec, PersistentRecord};

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



/// Unit key type for the single "sum" entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CumSumKey;



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
    type Error = std::convert::Infallible;

    fn extract(ctx: &Context, prior: &CumulativeSum) -> Result<u64, Self::Error> {
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
    fn into_entries(sum: CumulativeSum) -> Vec<(Self::Key, Self::Value)> {
        vec![(CumSumKey, sum)]
    }

    fn from_entries(entries: Vec<(Self::Key, Self::Value)>) -> CumulativeSum {
        entries
            .into_iter()
            .next()
            .map(|(_, v)| v)
            .unwrap_or(CumulativeSum::new(0))
    }
}

impl EntryCodec for CumulativeSumIndex {
    type Key = CumSumKey;
    type Value = CumulativeSum;
    type PersistentKey = PersistentCumSumKey;
    type PersistentValue = PersistentCumulativeSum;

    fn fingerprint_samples() -> Vec<(CumSumKey, CumulativeSum)> {
        vec![(CumSumKey, CumulativeSum(1))]
    }
}

/// On-disk record for [`CumSumKey`].
#[derive(PersistentRecord)]
pub struct PersistentCumSumKey([u8; 3]);

impl PersistentRecord for PersistentCumSumKey {
    type Domain = CumSumKey;

    fn from_domain(_domain: &CumSumKey) -> Self {
        Self(*b"sum")
    }
    fn into_domain(self) -> Result<CumSumKey, PersistDecodeError> {
        if self.0 == *b"sum" {
            Ok(CumSumKey)
        } else {
            Err(PersistDecodeError::Invalid("not the sum key".to_owned()))
        }
    }
}

/// On-disk record for [`CumulativeSum`].
#[derive(PersistentRecord)]
pub struct PersistentCumulativeSum(u64);

impl PersistentRecord for PersistentCumulativeSum {
    type Domain = CumulativeSum;

    fn from_domain(domain: &CumulativeSum) -> Self {
        Self(domain.0)
    }
    fn into_domain(self) -> Result<CumulativeSum, PersistDecodeError> {
        Ok(CumulativeSum(self.0))
    }
}
