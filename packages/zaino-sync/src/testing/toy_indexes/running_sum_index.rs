//! BlockLocal × Fold: running sum of values across blocks in a batch.

use crate::descriptor::{BlockLocal, Fold};
use crate::encode::Encode;
use crate::primitives::IndexId;
use crate::traits::{ExtractError, ExtractLocal, IndexDef, MergeFold, Schema};

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

    /// The raw numeric value.
    pub const fn value(&self) -> u64 {
        self.0
    }
}

impl Encode for RunningSum {
    fn encode(&self) -> Vec<u8> {
        self.0.to_le_bytes().to_vec()
    }
}

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
    fn extract(ctx: &Context) -> Result<Self::Delta, ExtractError> {
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
    type Key = &'static [u8];
    type Value = RunningSum;

    fn into_entries(sum: RunningSum) -> Vec<(Self::Key, Self::Value)> {
        vec![(b"sum".as_slice(), sum)]
    }
}
