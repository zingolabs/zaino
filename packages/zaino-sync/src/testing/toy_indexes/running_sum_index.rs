//! BlockLocal × Fold: running sum of values across blocks in a batch.

use crate::descriptor::{BlockLocal, Fold};
use crate::encode::{Decode, DecodeError, Encode};
use crate::primitives::IndexId;
use crate::traits::{ExtractLocal, IndexDef, MergeFold, Schema};
use zaino_persistence_codec::{DecodeError as PersistDecodeError, EntryCodec, PersistentRecord};

/// Block context for this index: just the block's value.
pub struct Context {
    /// Arbitrary value carried by this block.
    pub value: u32,
}

/// A running sum of block values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunningSum(u64);

impl RunningSum {
    /// Create a running sum.
    pub const fn new(sum: u64) -> Self {
        Self(sum)
    }
}

impl Encode for RunningSum {
    fn encode(&self) -> Vec<u8> {
        self.0.to_le_bytes().to_vec()
    }
}

impl Decode for RunningSum {
    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        Ok(Self(u64::decode(bytes)?))
    }
}

/// Unit key type for the single "sum" entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SumKey;

impl Encode for SumKey {
    fn encode(&self) -> Vec<u8> {
        b"sum".to_vec()
    }
}

impl Decode for SumKey {
    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        if bytes == b"sum" {
            Ok(Self)
        } else {
            Err(DecodeError::Failed("expected 'sum' key".into()))
        }
    }
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
    type Error = std::convert::Infallible;

    fn extract(ctx: &Context) -> Result<Self::Delta, Self::Error> {
        Ok(u64::from(ctx.value))
    }
}

impl MergeFold for RunningSumIndex {
    type FoldState = RunningSum;

    fn initial_state() -> Self::FoldState {
        RunningSum::new(0)
    }

    fn fold(state: &mut Self::FoldState, delta: Self::Delta) {
        state.0 += delta;
    }
}

impl Schema<RunningSum> for RunningSumIndex {
    fn into_entries(sum: RunningSum) -> Vec<(Self::Key, Self::Value)> {
        vec![(SumKey, sum)]
    }

    fn from_entries(entries: Vec<(Self::Key, Self::Value)>) -> RunningSum {
        entries
            .into_iter()
            .next()
            .map(|(_, v)| v)
            .unwrap_or(RunningSum::new(0))
    }
}

impl EntryCodec for RunningSumIndex {
    type Key = SumKey;
    type Value = RunningSum;
    type PersistentKey = PersistentSumKey;
    type PersistentValue = PersistentRunningSum;

    fn fingerprint_samples() -> Vec<(SumKey, RunningSum)> {
        vec![(SumKey, RunningSum(1))]
    }
}

/// On-disk record for [`SumKey`].
pub struct PersistentSumKey(SumKey);

impl PersistentRecord for PersistentSumKey {
    type Domain = SumKey;

    fn from_domain(domain: &SumKey) -> Self {
        Self(*domain)
    }
    fn into_domain(self) -> Result<SumKey, PersistDecodeError> {
        Ok(self.0)
    }
    fn encode(&self) -> Vec<u8> {
        Encode::encode(&self.0)
    }
    fn decode(bytes: &[u8]) -> Result<Self, PersistDecodeError> {
        <SumKey as Decode>::decode(bytes)
            .map(Self)
            .map_err(|e| PersistDecodeError::Invalid(e.to_string()))
    }
}

/// On-disk record for [`RunningSum`].
pub struct PersistentRunningSum(RunningSum);

impl PersistentRecord for PersistentRunningSum {
    type Domain = RunningSum;

    fn from_domain(domain: &RunningSum) -> Self {
        Self(*domain)
    }
    fn into_domain(self) -> Result<RunningSum, PersistDecodeError> {
        Ok(self.0)
    }
    fn encode(&self) -> Vec<u8> {
        Encode::encode(&self.0)
    }
    fn decode(bytes: &[u8]) -> Result<Self, PersistDecodeError> {
        <RunningSum as Decode>::decode(bytes)
            .map(Self)
            .map_err(|e| PersistDecodeError::Invalid(e.to_string()))
    }
}
