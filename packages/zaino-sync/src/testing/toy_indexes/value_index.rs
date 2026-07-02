//! BlockLocal × Append: stores (height → value) for each block.

use crate::descriptor::{
    Append, BlockLocal, CompositionType, Descriptor, InputScope, SourceAccess,
};
use crate::primitives::IndexId;
use crate::traits::{ExtractError, ExtractLocal, IndexDef, MergeAppend, WriteOp};

/// Block context for this index: height and value.
pub struct Context {
    /// Block height.
    pub height: u64,
    /// Arbitrary value carried by this block.
    pub value: u32,
}

/// Stores (height → value) for each block.
pub struct ValueIndex;

/// Index identity.
pub const ID: IndexId = IndexId::new("value");

impl IndexDef for ValueIndex {
    type Scope = BlockLocal;
    type Composition = Append;
    type Delta = Vec<(Vec<u8>, Vec<u8>)>;
    type BlockContext = Context;

    fn descriptor() -> Descriptor {
        Descriptor {
            name: ID,
            scope: InputScope::BlockLocal,
            composition: CompositionType::Append,
            dependencies: &[],
            source_access: SourceAccess::None,
        }
    }
}

impl ExtractLocal for ValueIndex {
    fn extract(ctx: &Context) -> Result<Self::Delta, ExtractError> {
        Ok(vec![(
            ctx.height.to_le_bytes().to_vec(),
            ctx.value.to_le_bytes().to_vec(),
        )])
    }
}

impl MergeAppend for ValueIndex {
    fn to_write_ops(delta: Self::Delta) -> Vec<WriteOp> {
        delta
            .into_iter()
            .map(|(key, value)| WriteOp::Put {
                index: ID,
                key,
                value,
            })
            .collect()
    }
}
