//! BlockLocal × Monoidal: counts total blocks seen in each batch.

use crate::descriptor::{BlockLocal, Monoidal};
use crate::encode::{Decode, DecodeError, Encode};
use crate::primitives::IndexId;
use crate::traits::{ExtractError, ExtractLocal, IndexDef, MergeMonoidal, Schema, SchemaDecodeError};

/// Block context for this index: nothing needed.
///
/// CountIndex only counts blocks — it reads no data from the block.
/// Using `()` means any set-wide context satisfies it via a trivial
/// `ProvideContext<()>` impl.
pub type Context = ();

/// A count of blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockCount(u64);

impl BlockCount {
    /// Create a block count.
    pub const fn new(count: u64) -> Self {
        Self(count)
    }

    /// The raw numeric value.
    pub const fn value(&self) -> u64 {
        self.0
    }
}

impl Encode for BlockCount {
    fn encode(&self) -> Vec<u8> {
        self.0.to_le_bytes().to_vec()
    }
}

impl Decode for BlockCount {
    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        Ok(Self(u64::decode(bytes)?))
    }
}

/// Unit key type for the single "total" entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TotalKey;

impl Encode for TotalKey {
    fn encode(&self) -> Vec<u8> {
        b"total".to_vec()
    }
}

impl Decode for TotalKey {
    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        if bytes == b"total" {
            Ok(Self)
        } else {
            Err(DecodeError::Failed("expected 'total' key".into()))
        }
    }
}

/// Counts total blocks seen in each batch.
pub struct CountIndex;

/// Index identity.
pub const ID: IndexId = IndexId::new("count");

impl IndexDef for CountIndex {
    type Scope = BlockLocal;
    type Composition = Monoidal;
    type Delta = BlockCount;
    type BlockContext = Context;

    const NAME: IndexId = ID;
}

impl ExtractLocal for CountIndex {
    fn extract(_ctx: &Context) -> Result<Self::Delta, ExtractError> {
        Ok(BlockCount::new(1))
    }
}

impl MergeMonoidal for CountIndex {
    type Accumulator = BlockCount;

    fn identity() -> Self::Accumulator {
        BlockCount::new(0)
    }

    fn lift(delta: Self::Delta) -> Self::Accumulator {
        delta
    }

    fn combine(a: Self::Accumulator, b: Self::Accumulator) -> Self::Accumulator {
        BlockCount::new(a.0 + b.0)
    }
}

impl Schema<BlockCount> for CountIndex {
    type Key = TotalKey;
    type Value = BlockCount;

    fn into_entries(count: BlockCount) -> Vec<(Self::Key, Self::Value)> {
        vec![(TotalKey, count)]
    }

    fn from_entries(entries: Vec<(Self::Key, Self::Value)>) -> BlockCount {
        entries
            .into_iter()
            .next()
            .map(|(_, v)| v)
            .unwrap_or(BlockCount::new(0))
    }

    fn encode_key(key: &Self::Key) -> Vec<u8> { key.encode() }
    fn encode_value(value: &Self::Value) -> Vec<u8> { value.encode() }
    fn decode_key(bytes: &[u8]) -> Result<Self::Key, SchemaDecodeError> {
        TotalKey::decode(bytes).map_err(|e| SchemaDecodeError::Invalid(e.to_string()))
    }
    fn decode_value(bytes: &[u8]) -> Result<Self::Value, SchemaDecodeError> {
        BlockCount::decode(bytes).map_err(|e| SchemaDecodeError::Invalid(e.to_string()))
    }
}
