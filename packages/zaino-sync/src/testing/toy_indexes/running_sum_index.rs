//! BlockLocal × Fold: running sum of values across blocks in a batch.

use crate::descriptor::{BlockLocal, Fold};
use crate::primitives::IndexId;
use crate::traits::{ExtractError, ExtractLocal, IndexDef, MergeFold, Schema};

/// Block context for this index: just the block's value.
pub struct Context {
    /// Arbitrary value carried by this block.
    pub value: u32,
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
    type FoldState = u64;

    fn initial_state() -> Self::FoldState {
        0
    }

    fn fold(state: &mut Self::FoldState, delta: Self::Delta) {
        *state += delta;
    }
}

impl Schema<u64> for RunningSumIndex {
    type Key = &'static [u8];
    type Value = u64;

    fn into_entries(sum: u64) -> Vec<(Self::Key, Self::Value)> {
        vec![(b"sum".as_slice(), sum)]
    }
}
