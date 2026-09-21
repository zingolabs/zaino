//! BlockLocal × Monoidal: counts total blocks seen in each batch.

use crate::descriptor::{BlockLocal, Monoidal};
use crate::encode::{Decode, DecodeError, Encode};
use crate::primitives::IndexId;
use crate::traits::{ExtractLocal, IndexDef, MergeMonoidal, Schema};
use zaino_persistence_codec::{DecodeError as PersistDecodeError, EntryCodec, PersistentRecord};

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
    type Error = std::convert::Infallible;

    fn extract(_ctx: &Context) -> Result<Self::Delta, Self::Error> {
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
}

impl EntryCodec for CountIndex {
    type Key = TotalKey;
    type Value = BlockCount;
    type PersistentKey = PersistentTotalKey;
    type PersistentValue = PersistentBlockCount;

    fn fingerprint_samples() -> Vec<(TotalKey, BlockCount)> {
        vec![(TotalKey, BlockCount(1))]
    }
}

/// On-disk record for [`TotalKey`].
pub struct PersistentTotalKey(TotalKey);

impl PersistentRecord for PersistentTotalKey {
    type Domain = TotalKey;

    fn from_domain(domain: &TotalKey) -> Self {
        Self(*domain)
    }
    fn into_domain(self) -> Result<TotalKey, PersistDecodeError> {
        Ok(self.0)
    }
    fn encode(&self) -> Vec<u8> {
        Encode::encode(&self.0)
    }
    fn decode(bytes: &[u8]) -> Result<Self, PersistDecodeError> {
        <TotalKey as Decode>::decode(bytes)
            .map(Self)
            .map_err(|e| PersistDecodeError::Invalid(e.to_string()))
    }
}

/// On-disk record for [`BlockCount`].
pub struct PersistentBlockCount(BlockCount);

impl PersistentRecord for PersistentBlockCount {
    type Domain = BlockCount;

    fn from_domain(domain: &BlockCount) -> Self {
        Self(*domain)
    }
    fn into_domain(self) -> Result<BlockCount, PersistDecodeError> {
        Ok(self.0)
    }
    fn encode(&self) -> Vec<u8> {
        Encode::encode(&self.0)
    }
    fn decode(bytes: &[u8]) -> Result<Self, PersistDecodeError> {
        <BlockCount as Decode>::decode(bytes)
            .map(Self)
            .map_err(|e| PersistDecodeError::Invalid(e.to_string()))
    }
}
