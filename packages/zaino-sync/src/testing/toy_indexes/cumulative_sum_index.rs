//! SelfCumulative x Monoidal: running sum where extraction depends on
//! the accumulated state.
//!
//! Blocks whose prior running total exceeds a threshold contribute
//! double their value. This makes extraction genuinely dependent on
//! prior state — a BlockLocal index could not reproduce the same
//! result.

use crate::descriptor::{Monoidal, SelfCumulative};
use crate::encode::{Decode, DecodeError, Encode};
use crate::primitives::IndexId;
use crate::traits::{
    ExtractCumulative, ExtractError, IndexDef, MergeMonoidal, Schema, SchemaDecodeError,
};

/// Block context for this index: just the block's value.
pub struct Context {
    /// Arbitrary value carried by this block.
    pub value: u32,
}

/// The accumulated sum — serves as both PriorState and Accumulator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CumulativeSum(u64);

impl CumulativeSum {
    /// Create a cumulative sum.
    pub const fn new(sum: u64) -> Self {
        Self(sum)
    }

    /// The raw numeric value.
    pub const fn value(&self) -> u64 {
        self.0
    }
}

impl Encode for CumulativeSum {
    fn encode(&self) -> Vec<u8> {
        self.0.to_le_bytes().to_vec()
    }
}

impl Decode for CumulativeSum {
    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        Ok(Self(u64::decode(bytes)?))
    }
}

/// Unit key type for the single "sum" entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CumSumKey;

impl Encode for CumSumKey {
    fn encode(&self) -> Vec<u8> {
        b"sum".to_vec()
    }
}

impl Decode for CumSumKey {
    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        if bytes == b"sum" {
            Ok(Self)
        } else {
            Err(DecodeError::Failed("expected 'sum' key".into()))
        }
    }
}

/// Cumulative sum where blocks past a threshold contribute double.
pub struct CumulativeSumIndex;

/// Index identity.
pub const ID: IndexId = IndexId::new("cumulative_sum");

/// Prior sums above this value cause blocks to contribute double.
const DOUBLING_THRESHOLD: u64 = 10;

impl IndexDef for CumulativeSumIndex {
    type Scope = SelfCumulative;
    type Composition = Monoidal;
    type Delta = u64;
    type BlockContext = Context;

    const NAME: IndexId = ID;
}

impl ExtractCumulative for CumulativeSumIndex {
    type PriorState = CumulativeSum;

    fn extract(ctx: &Context, prior: &CumulativeSum) -> Result<u64, ExtractError> {
        let base = u64::from(ctx.value);
        if prior.value() > DOUBLING_THRESHOLD {
            Ok(base * 2)
        } else {
            Ok(base)
        }
    }
}

impl MergeMonoidal for CumulativeSumIndex {
    type Accumulator = CumulativeSum;

    fn identity() -> CumulativeSum {
        CumulativeSum::new(0)
    }

    fn lift(delta: u64) -> CumulativeSum {
        CumulativeSum::new(delta)
    }

    fn combine(a: CumulativeSum, b: CumulativeSum) -> CumulativeSum {
        CumulativeSum::new(a.0 + b.0)
    }
}

impl Schema<CumulativeSum> for CumulativeSumIndex {
    type Key = CumSumKey;
    type Value = CumulativeSum;

    fn into_entries(sum: CumulativeSum) -> Vec<(Self::Key, Self::Value)> {
        vec![(CumSumKey, sum)]
    }

    fn from_entries(entries: Vec<(Self::Key, Self::Value)>) -> CumulativeSum {
        entries
            .into_iter()
            .next()
            .map(|(_, v)| v)
            .unwrap_or(CumulativeSum::new(0))
    }

    fn encode_key(key: &Self::Key) -> Vec<u8> { key.encode() }
    fn encode_value(value: &Self::Value) -> Vec<u8> { value.encode() }
    fn decode_key(bytes: &[u8]) -> Result<Self::Key, SchemaDecodeError> {
        CumSumKey::decode(bytes).map_err(|e| SchemaDecodeError::Invalid(e.to_string()))
    }
    fn decode_value(bytes: &[u8]) -> Result<Self::Value, SchemaDecodeError> {
        CumulativeSum::decode(bytes).map_err(|e| SchemaDecodeError::Invalid(e.to_string()))
    }
}
