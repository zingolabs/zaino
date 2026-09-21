//! SelfCumulative × Append: a per-height running sum.
//!
//! The archetype the [`CumulativeAppendBridge`](crate::bridge) exists for:
//! extraction depends on the prior running total (scope is `SelfCumulative`),
//! but each height emits its own disjoint `height → total` entry (composition is
//! `Append`). Unlike [`CumulativeSumIndex`](super::cumulative_sum_index), which
//! collapses to a single tip total, this keeps the whole series — the value at
//! every height.

use crate::descriptor::{Append, SelfCumulative};
use crate::primitives::{BlockHeight, IndexId};
use crate::traits::{CumulativeAppend, ExtractCumulative, IndexDef, MergeAppend, Schema};
use zaino_persistence_codec::keys::HeightKey;
use zaino_persistence_codec::layout::{Cursor, Writer};
use zaino_persistence_codec::{DecodeError as PersistDecodeError, EntryCodec, PersistentRecord};

/// Block context: the block's height and its value.
pub struct Context {
    /// Block height (the entry key).
    pub height: BlockHeight,
    /// Value this block contributes to the running sum.
    pub value: u32,
}

/// The running sum through a given height — both the carry and the stored value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunningTotal(pub u64);

/// One height's entry: the running sum after this block.
pub struct SeriesEntry {
    /// Block height (key).
    pub height: BlockHeight,
    /// Running sum after this block (value, and the carry to the next height).
    pub running: RunningTotal,
}

/// Per-height cumulative sum index.
pub struct CumulativeSeriesIndex;

/// Index identity.
pub const ID: IndexId = IndexId::new("cumulative_series");

impl IndexDef for CumulativeSeriesIndex {
    type Scope = SelfCumulative;
    type Composition = Append;
    type Delta = SeriesEntry;
    type BlockContext = Context;

    const NAME: IndexId = ID;
}

impl ExtractCumulative for CumulativeSeriesIndex {
    type PriorState = RunningTotal;
    type Error = std::convert::Infallible;

    fn extract(ctx: &Context, prior: &RunningTotal) -> Result<SeriesEntry, Self::Error> {
        Ok(SeriesEntry {
            height: ctx.height,
            running: RunningTotal(prior.0 + u64::from(ctx.value)),
        })
    }
}

impl MergeAppend for CumulativeSeriesIndex {}

impl CumulativeAppend for CumulativeSeriesIndex {
    fn initial_carry() -> RunningTotal {
        RunningTotal(0)
    }

    fn carry(delta: &SeriesEntry) -> RunningTotal {
        delta.running
    }
}

impl Schema<Vec<SeriesEntry>> for CumulativeSeriesIndex {
    fn into_entries(entries: Vec<SeriesEntry>) -> Vec<(Self::Key, Self::Value)> {
        entries.into_iter().map(|e| (e.height, e.running)).collect()
    }

    fn from_entries(entries: Vec<(Self::Key, Self::Value)>) -> Vec<SeriesEntry> {
        entries
            .into_iter()
            .map(|(height, running)| SeriesEntry { height, running })
            .collect()
    }
}

impl EntryCodec for CumulativeSeriesIndex {
    type Key = BlockHeight;
    type Value = RunningTotal;
    // The key is a plain block height — reuse the shared height record.
    type PersistentKey = HeightKey<BlockHeight>;
    type PersistentValue = PersistentRunningTotal;

    fn fingerprint_samples() -> Vec<(BlockHeight, RunningTotal)> {
        vec![(BlockHeight::new(1), RunningTotal(1))]
    }
}

/// On-disk record for [`RunningTotal`]: a single `u64` little-endian.
pub struct PersistentRunningTotal(u64);

impl PersistentRecord for PersistentRunningTotal {
    type Domain = RunningTotal;

    fn from_domain(domain: &RunningTotal) -> Self {
        Self(domain.0)
    }
    fn into_domain(self) -> Result<RunningTotal, PersistDecodeError> {
        Ok(RunningTotal(self.0))
    }
    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::with_capacity(8);
        writer.u64(self.0);
        writer.into_bytes()
    }
    fn decode(bytes: &[u8]) -> Result<Self, PersistDecodeError> {
        let mut cursor = Cursor::new(bytes);
        let raw = cursor.u64()?;
        cursor.finish()?;
        Ok(Self(raw))
    }
}
