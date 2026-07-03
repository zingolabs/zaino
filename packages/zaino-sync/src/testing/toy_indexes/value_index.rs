//! BlockLocal × Append: stores (height → value) for each block.

use crate::descriptor::{Append, BlockLocal};
use crate::primitives::{BlockHeight, IndexId};
use crate::traits::{ExtractError, ExtractLocal, IndexDef, MergeAppend, Schema};

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

impl MergeAppend for ValueIndex {}

impl Schema<Vec<Entry>> for ValueIndex {
    type Key = BlockHeight;
    type Value = u32;

    fn into_entries(entries: Vec<Entry>) -> Vec<(Self::Key, Self::Value)> {
        entries
            .into_iter()
            .map(|entry| (entry.height, entry.value))
            .collect()
    }
}
