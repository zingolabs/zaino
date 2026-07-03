//! BlockLocal × Append: stores (height → value) for each block.

use crate::descriptor::{Append, BlockLocal};
use crate::encode::{Decode, DecodeError, Encode};
use crate::primitives::{BlockHeight, IndexId};
use crate::traits::{ExtractError, ExtractLocal, IndexDef, MergeAppend, Schema};

/// Block context for this index: height and value.
pub struct Context {
    /// Block height.
    pub height: BlockHeight,
    /// Arbitrary value carried by this block.
    pub value: BlockValue,
}

/// An arbitrary value carried by a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockValue(u32);

impl BlockValue {
    /// Create a block value.
    pub const fn new(val: u32) -> Self {
        Self(val)
    }

    /// The raw numeric value.
    pub const fn value(&self) -> u32 {
        self.0
    }
}

impl Encode for BlockValue {
    fn encode(&self) -> Vec<u8> {
        self.0.to_le_bytes().to_vec()
    }
}

impl Decode for BlockValue {
    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        Ok(Self(u32::decode(bytes)?))
    }
}

/// A single height → value entry. Domain type — no serialization.
pub struct Entry {
    /// Block height.
    pub height: BlockHeight,
    /// Block value.
    pub value: BlockValue,
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
    type Value = BlockValue;

    fn into_entries(entries: Vec<Entry>) -> Vec<(Self::Key, Self::Value)> {
        entries
            .into_iter()
            .map(|entry| (entry.height, entry.value))
            .collect()
    }

    fn from_entries(entries: Vec<(Self::Key, Self::Value)>) -> Vec<Entry> {
        entries
            .into_iter()
            .map(|(height, value)| Entry { height, value })
            .collect()
    }
}
