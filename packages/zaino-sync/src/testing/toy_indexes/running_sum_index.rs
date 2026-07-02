//! BlockLocal × Fold: running sum of values across blocks in a batch.

use crate::descriptor::{BlockLocal, Fold};
use crate::primitives::IndexId;
use crate::traits::{ExtractError, ExtractLocal, IndexDef, MergeFold, WriteOp};

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

    fn to_write_ops(state: Self::FoldState) -> Vec<WriteOp> {
        vec![WriteOp::Put {
            index: ID,
            key: b"sum".to_vec(),
            value: state.to_le_bytes().to_vec(),
        }]
    }
}
