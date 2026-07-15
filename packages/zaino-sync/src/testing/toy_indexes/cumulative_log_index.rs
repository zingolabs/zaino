//! SelfCumulative x Append: a running log where extraction depends on prior state.
//!
//! Each block appends a "label" to the log. The label depends on the
//! current log length (prior state): blocks arriving when the log has
//! an even number of entries get prefixed with "E:", odd with "O:".
//! This makes extraction genuinely state-dependent.

use crate::descriptor::{Append, SelfCumulative};
use crate::encode::{Decode, DecodeError, Encode};
use crate::primitives::IndexId;
use crate::traits::{
    ExtractCumulative, ExtractError, IndexDef, MergeAppend, Schema, SchemaDecodeError,
};

/// Block context: the raw label for this block.
pub struct Context {
    /// The label text carried by this block.
    pub label: String,
}

/// A labeled entry in the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry(String);

impl LogEntry {
    /// Create a log entry.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// The label text.
    pub fn text(&self) -> &str {
        &self.0
    }
}

impl Encode for LogEntry {
    fn encode(&self) -> Vec<u8> {
        self.0.as_bytes().to_vec()
    }
}

impl Decode for LogEntry {
    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        String::from_utf8(bytes.to_vec())
            .map(Self)
            .map_err(|e| DecodeError::Failed(e.to_string()))
    }
}

/// Key: the log index (position in the cumulative log).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogIndex(u32);

impl LogIndex {
    /// Create a log index.
    pub fn new(i: u32) -> Self {
        Self(i)
    }

    /// The raw index value.
    pub fn value(&self) -> u32 {
        self.0
    }
}

impl Encode for LogIndex {
    fn encode(&self) -> Vec<u8> {
        self.0.to_le_bytes().to_vec()
    }
}

impl Decode for LogIndex {
    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        Ok(Self(u32::decode(bytes)?))
    }
}

/// The cumulative log index.
pub struct CumulativeLogIndex;

/// Index identity.
pub const ID: IndexId = IndexId::new("cumulative_log");

impl IndexDef for CumulativeLogIndex {
    type Scope = SelfCumulative;
    type Composition = Append;
    type Delta = LogEntry;
    type BlockContext = Context;

    const NAME: IndexId = ID;
}

impl ExtractCumulative for CumulativeLogIndex {
    /// Prior state is the accumulated list of deltas (Vec<LogEntry>).
    type PriorState = Vec<LogEntry>;

    fn extract(ctx: &Context, prior: &Vec<LogEntry>) -> Result<LogEntry, ExtractError> {
        let prefix = if prior.len() % 2 == 0 { "E" } else { "O" };
        Ok(LogEntry::new(format!("{prefix}:{}", ctx.label)))
    }
}

impl MergeAppend for CumulativeLogIndex {}

impl Schema<Vec<LogEntry>> for CumulativeLogIndex {
    type Key = LogIndex;
    type Value = LogEntry;

    fn into_entries(entries: Vec<LogEntry>) -> Vec<(Self::Key, Self::Value)> {
        entries
            .into_iter()
            .enumerate()
            .map(|(i, e)| (LogIndex::new(i as u32), e))
            .collect()
    }

    fn from_entries(entries: Vec<(Self::Key, Self::Value)>) -> Vec<LogEntry> {
        let mut sorted = entries;
        sorted.sort_by_key(|(k, _)| k.0);
        sorted.into_iter().map(|(_, v)| v).collect()
    }

    fn encode_key(key: &Self::Key) -> Vec<u8> { key.encode() }
    fn encode_value(value: &Self::Value) -> Vec<u8> { value.encode() }
    fn decode_key(bytes: &[u8]) -> Result<Self::Key, SchemaDecodeError> {
        LogIndex::decode(bytes).map_err(|e| SchemaDecodeError::Invalid(e.to_string()))
    }
    fn decode_value(bytes: &[u8]) -> Result<Self::Value, SchemaDecodeError> {
        LogEntry::decode(bytes).map_err(|e| SchemaDecodeError::Invalid(e.to_string()))
    }
}
