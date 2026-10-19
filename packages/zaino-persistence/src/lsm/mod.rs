//! Size-tiered LSM over immutable sorted segments: the map tables of [`DiskEngine`]
//!
//! - Stripped to what Zaino's indexes use: **nothing updated** (every row derives from one block)
//!   → no memtable, no WAL, no versions
//! - `deletes()` map: insert-once / remove-once → a tombstone row, cancelled with its value in any
//!   merge
//! - Segment = one batch, sorted by key, **fixed stride, 100% packed**, then per-block fences and
//!   a binary fuse filter (`layout.rs`)
//! - Durable segments = owner's manifest list (`SegmentMeta`: id, record count, file seal)
//! - Segment file the manifest does not list = not yet durable, removed at open
//! - Integrity = the file's page checksums (`crate::pages`)
//! - Merge = pure k-way merge on a background thread, one per size tier (Lucene
//!   `TieredMergePolicy`, `policy.rs`), swapped in by the owner's next manifest (`SegmentSet`)
//! - Merge = read-time cost only (segments = state, not a log: unmerged set never slows a reopen)
//!
//! [`DiskEngine`]: crate::DiskEngine

mod dir;
mod file;
mod filter;
mod layout;
mod meta;
mod policy;
mod reader;
mod segment_set;
mod slots;
mod spill;
mod telemetry;
mod writer;

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) use dir::file_name;
pub(crate) use dir::files;
pub(crate) use meta::{decode_list, encode_list, SegmentMeta};
pub(crate) use policy::stall_at;
pub(crate) use reader::SegmentsView;
pub(crate) use segment_set::SegmentSet;
pub(crate) use slots::Slots;
pub(crate) use telemetry::{describe_metrics, metric_names};

use std::num::{NonZeroU32, NonZeroUsize};

use serde::{Deserialize, Serialize};

use crate::pages::PageError;

/// Map tables' merge policy, one per [`crate::DiskEngine`] (its stores share `merge_slots`)
///
/// - `fanout` same-tier segments merge into one: write amplification `log_fanout(rows)`
/// - `merge_slots` = merges doing I/O at once (the rest wait, lowest tier first)
/// - `merge_mib_per_sec` = their read + written bytes per second, shared (commits never paced)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct LsmConfig {
    pub fanout: Fanout,
    pub merge_slots: NonZeroUsize,
    pub merge_mib_per_sec: NonZeroU32,
}

/// Same-tier segments one merge takes, >= 2 (a merge of one segment = no merge)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "usize", into = "usize")]
pub struct Fanout(usize);

impl Fanout {
    pub const fn new(segments: usize) -> Option<Self> {
        match segments >= 2 {
            true => Some(Self(segments)),
            false => None,
        }
    }

    pub const fn get(self) -> usize {
        self.0
    }
}

impl TryFrom<usize> for Fanout {
    type Error = String;

    fn try_from(segments: usize) -> std::result::Result<Self, String> {
        Self::new(segments).ok_or_else(|| format!("fanout {segments}: a merge needs 2 segments"))
    }
}

impl From<Fanout> for usize {
    fn from(fanout: Fanout) -> Self {
        fanout.0
    }
}

impl Default for LsmConfig {
    fn default() -> Self {
        Self {
            fanout: Fanout(16),
            merge_slots: NonZeroUsize::new(4).expect("4 is non-zero"),
            merge_mib_per_sec: NonZeroU32::new(200).expect("200 is non-zero"),
        }
    }
}

/// Why a segment could not be read or written
#[derive(Debug, thiserror::Error)]
pub enum SegmentError {
    #[error("segment io: {0}")]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Page(#[from] PageError),

    #[error("segment {segment} filter build failed: {reason}")]
    Filter { segment: u32, reason: &'static str },

    /// Rows breaking the insert-once / remove-once contract met in a merge (a writer bug)
    #[error("merge into segment {segment}: {reason}")]
    Contract { segment: u32, reason: String },
}

type Result<T> = std::result::Result<T, SegmentError>;
