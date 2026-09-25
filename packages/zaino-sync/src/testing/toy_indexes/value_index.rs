//! BlockLocal × Append: stores (height → value) for each block.

use crate::descriptor::{Append, BlockLocal};
use crate::primitives::{BlockHeight, IndexId};
use crate::traits::{ExtractLocal, IndexDef, MergeAppend, Schema};
use zaino_persistence_codec::keys::HeightKey;
use zaino_persistence_codec::{DecodeError as PersistDecodeError, EntryCodec, PersistentRecord};

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
    type Error = std::convert::Infallible;

    fn extract(ctx: &Context) -> Result<Self::Delta, Self::Error> {
        Ok(Entry {
            height: ctx.height,
            value: ctx.value,
        })
    }
}

impl MergeAppend for ValueIndex {}

impl Schema<Vec<Entry>> for ValueIndex {
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

impl EntryCodec for ValueIndex {
    type Key = BlockHeight;
    type Value = BlockValue;
    // The key is a plain block height — reuse the shared height record.
    type PersistentKey = HeightKey<BlockHeight>;
    type PersistentValue = PersistentBlockValue;

    fn fingerprint_samples() -> Vec<(BlockHeight, BlockValue)> {
        vec![(BlockHeight::new(1), BlockValue(2))]
    }
}

/// On-disk record for [`BlockValue`].
#[derive(PersistentRecord)]
pub struct PersistentBlockValue(u32);

impl PersistentRecord for PersistentBlockValue {
    type Domain = BlockValue;

    fn from_domain(domain: &BlockValue) -> Self {
        Self(domain.0)
    }
    fn into_domain(self) -> Result<BlockValue, PersistDecodeError> {
        Ok(BlockValue(self.0))
    }
}
