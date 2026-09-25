//! BlockLocal × Fold: running sum of values across blocks in a batch.

use crate::descriptor::{BlockLocal, Fold};
use crate::primitives::IndexId;
use crate::traits::{ExtractLocal, IndexDef, MergeFold, Schema};
use zaino_persistence_codec::{DecodeError as PersistDecodeError, EntryCodec, PersistentRecord};

/// Block context for this index: just the block's value.
pub struct Context {
    /// Arbitrary value carried by this block.
    pub value: u32,
}

/// A running sum of block values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunningSum(u64);

impl RunningSum {
    /// Create a running sum.
    pub const fn new(sum: u64) -> Self {
        Self(sum)
    }
}



/// Unit key type for the single "sum" entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SumKey;



/// Running sum of values across blocks in a batch.
pub struct RunningSumIndex;

/// Index identity.
pub const ID: IndexId = IndexId::new("running_sum");

impl IndexDef for RunningSumIndex {
    type Scope = BlockLocal;
    type Composition = Fold;
    type Delta = u64;
    type BlockContext = Context;

    const NAME: IndexId = ID;
}

impl ExtractLocal for RunningSumIndex {
    type Error = std::convert::Infallible;

    fn extract(ctx: &Context) -> Result<Self::Delta, Self::Error> {
        Ok(u64::from(ctx.value))
    }
}

impl MergeFold for RunningSumIndex {
    type FoldState = RunningSum;

    fn initial_state() -> Self::FoldState {
        RunningSum::new(0)
    }

    fn fold(state: &mut Self::FoldState, delta: Self::Delta) {
        state.0 += delta;
    }
}

impl Schema<RunningSum> for RunningSumIndex {
    fn into_entries(sum: RunningSum) -> Vec<(Self::Key, Self::Value)> {
        vec![(SumKey, sum)]
    }

    fn from_entries(entries: Vec<(Self::Key, Self::Value)>) -> RunningSum {
        entries
            .into_iter()
            .next()
            .map(|(_, v)| v)
            .unwrap_or(RunningSum::new(0))
    }
}

impl EntryCodec for RunningSumIndex {
    type Key = SumKey;
    type Value = RunningSum;
    type PersistentKey = PersistentSumKey;
    type PersistentValue = PersistentRunningSum;

    fn fingerprint_samples() -> Vec<(SumKey, RunningSum)> {
        vec![(SumKey, RunningSum(1))]
    }
}

/// On-disk record for [`SumKey`].
#[derive(PersistentRecord)]
pub struct PersistentSumKey([u8; 3]);

impl PersistentRecord for PersistentSumKey {
    type Domain = SumKey;

    fn from_domain(_domain: &SumKey) -> Self {
        Self(*b"sum")
    }
    fn into_domain(self) -> Result<SumKey, PersistDecodeError> {
        if self.0 == *b"sum" {
            Ok(SumKey)
        } else {
            Err(PersistDecodeError::Invalid("not the sum key".to_owned()))
        }
    }
}

/// On-disk record for [`RunningSum`].
#[derive(PersistentRecord)]
pub struct PersistentRunningSum(u64);

impl PersistentRecord for PersistentRunningSum {
    type Domain = RunningSum;

    fn from_domain(domain: &RunningSum) -> Self {
        Self(domain.0)
    }
    fn into_domain(self) -> Result<RunningSum, PersistDecodeError> {
        Ok(RunningSum(self.0))
    }
}
