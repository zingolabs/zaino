//! BlockLocal × Append: stores (height → value) for each block.

use crate::descriptor::{Append, BlockLocal};
use crate::primitives::{BlockHeight, IndexId};
use crate::traits::{ExtractError, ExtractLocal, IndexDef, MergeAppend, WriteOp};

/// Block context for this index: height and value.
pub struct Context {
    /// Block height.
    pub height: BlockHeight,
    /// Arbitrary value carried by this block.
    pub value: u32,
}

/// A single height → value entry. Domain type — no serialization.
pub struct Entry {
    /// Block height.
    pub height: BlockHeight,
    /// Block value.
    pub value: u32,
}

/// Stores (height → value) for each block.
pub struct ValueIndex;

/// Index identity.
pub const ID: IndexId = IndexId::new("value");

impl IndexDef for ValueIndex {
    type Scope = BlockLocal;
    type Composition = Append;
    type Delta = Entry;
    type BlockContext = Context;

    const NAME: IndexId = ID;
}

impl ExtractLocal for ValueIndex {
    fn extract(ctx: &Context) -> Result<Self::Delta, ExtractError> {
        Ok(Entry {
            height: ctx.height,
            value: ctx.value,
        })
    }
}

impl MergeAppend for ValueIndex {
    fn to_write_ops(delta: Self::Delta) -> Vec<WriteOp> {
        vec![WriteOp::Put {
            index: ID,
            key: delta.height.value().to_le_bytes().to_vec(),
            value: delta.value.to_le_bytes().to_vec(),
        }]
    }
}
