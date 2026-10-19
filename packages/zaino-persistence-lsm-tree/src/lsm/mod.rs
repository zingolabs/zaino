//! Size-tiered LSM over immutable sorted segments: the map namespaces of the backend
//!
//! - Stripped to what the port's maps allow: **nothing updated** (each key inserted once) → no
//!   memtable, no WAL, no versions
//! - `deletes` map: insert-once / remove-once → a tombstone row, cancelled with its value in any
//!   merge (`docs/lsm-deletes.md`)
//! - Segment = one batch, sorted by key, **fixed stride, 100% packed**, then per-block fences and
//!   a binary fuse filter (`layout.rs`); read-only once sealed
//! - Committed segments = owner's manifest list (`SegmentMeta`: id, record count, file seal)
//! - Segment file the manifest does not list = uncommitted, removed at open
//! - Integrity = the file's page checksums (`crate::pages`)
//! - Merge = pure k-way merge on a background thread, one per size tier (Lucene
//!   `TieredMergePolicy`), swapped in by the owner's next manifest (`SegmentLog`)
//! - Reads = point lookups + a resumable k-way [`Cursor`] over borrowed rows (`reader.rs`)
//!
//! Design: `docs/index-data-structures.md`, `docs/durability.md`

mod emit;
mod file;
mod filter;
mod layout;
mod log;
mod meta;
mod reader;
mod report;
mod slots;
mod spill;
mod writer;

#[cfg(test)]
#[allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
mod tests;

pub use emit::{describe_metrics, METRIC_BUCKETS};
pub(crate) use layout::Shape;
pub(crate) use log::SegmentLog;
pub(crate) use meta::{decode_list, encode_list, SegmentMeta};
pub(crate) use reader::{Cursor, Lookup, Snapshot};
pub(crate) use slots::Slots;

use std::num::{NonZeroU32, NonZeroUsize};

use crate::pages::PageError;

/// Map namespaces' merge policy, one per backend (every map shares `merge_slots`)
///
/// - `fanout` (>= 2) same-tier segments merge into one: write amplification `log_fanout(rows)`
/// - `merge_slots` = merges doing I/O at once (the rest wait, lowest tier first)
/// - `merge_mib_per_sec` = their read + written bytes per second, shared (commits never paced)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LsmConfig {
    /// Same-tier segments merged into one (>= 2)
    pub fanout: usize,
    /// Merges doing I/O at once
    pub merge_slots: NonZeroUsize,
    /// Merge read + write bandwidth, MiB per second, shared
    pub merge_mib_per_sec: NonZeroU32,
}

impl Default for LsmConfig {
    fn default() -> Self {
        Self {
            fanout: 16,
            merge_slots: NonZeroUsize::new(4).unwrap_or(NonZeroUsize::MIN),
            merge_mib_per_sec: NonZeroU32::new(200).unwrap_or(NonZeroU32::MIN),
        }
    }
}

/// Per-namespace storage tuning: how, not what, a namespace stores (never changes an answer)
///
/// - `scope` = leading key bytes every range read shares (0 = keys read whole): the map's filter
///   covers those bytes, so a range within one scope skips segments without it
/// - `cache_writes` = written data kept in page cache (default: dropped once synced, so bulk
///   writes never evict the data reads use)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Tuning {
    /// Filter prefix length (0 = whole key)
    pub scope: u32,
    /// Keep written pages cached
    pub cache_writes: bool,
}

/// Why a segment could not be read or written
#[derive(Debug, thiserror::Error)]
pub(crate) enum SegmentError {
    #[error("segment io: {0}")]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Page(#[from] PageError),

    #[error("segment {segment} filter build failed: {reason}")]
    Filter { segment: u32, reason: &'static str },

    /// Rows breaking the insert-once / remove-once contract (a writer bug, or a caller's)
    #[error("segments {segments:?}: {breach}")]
    Contract { segments: Vec<u32>, breach: Breach },

    /// Bytes whose checksums pass but whose layout does not decode (a writer bug)
    #[error("segment {segment}: {reason}")]
    Corrupt { segment: u32, reason: &'static str },

    /// A map the LSM cannot hold
    #[error("map {name}: {reason}")]
    Shape { name: String, reason: String },

    /// A sealed segment that does not read back as written (a writer bug)
    #[error("segment {segment}: read back {reason}")]
    SelfCheck { segment: u32, reason: String },

    /// A merge thread panicked (outside this crate's code: it never panics)
    #[error("map {name}: a merge thread panicked")]
    MergePanicked { name: String },

    /// The engine's own state broke a precondition (a bug in this crate)
    #[error("lsm: {0}")]
    Internal(&'static str),

    #[error("{0} overflows")]
    Overflow(&'static str),
}

/// How rows broke the insert-once / remove-once contract
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Breach {
    /// A segment's rows not strictly ascending (a key written twice)
    #[error("rows not strictly ascending")]
    Unsorted,
    /// A key in `held` rows (only a value + its tombstone may share a key)
    #[error("a key held {held} times")]
    Repeated { held: usize },
}

type Result<T> = std::result::Result<T, SegmentError>;

/// `<id:010>.seg`, zero-padded so a listing sorts by id
pub(crate) fn file_name(segment: u32) -> String {
    format!("{segment:010}.seg")
}

/// Id of a segment file or its checksums; `None` for anything else in the directory
fn parse_file_name(name: &str) -> Option<u32> {
    name.strip_suffix(".crc")
        .unwrap_or(name)
        .strip_suffix(".seg")?
        .parse()
        .ok()
}
