//! Size-tiered LSM over immutable sorted segments: the map tables of [`DiskEngine`]
//!
//! - Stripped to what Zaino's indexes use: **nothing updated or deleted** (every row derives from
//!   one block) → no memtable, no WAL, no tombstones, no versions
//! - Segment = one batch, sorted by key, **fixed stride, 100% packed**, then per-block fences and
//!   a binary fuse filter (`layout.rs`)
//! - Committed segments = the owner's manifest list ([`SegmentMeta`]: id, record count, file
//!   seal); a segment file the manifest does not list is uncommitted and removed at open
//! - Integrity = the file's page checksums (`crate::pages`)
//! - Merge = pure k-way merge on a background thread, one per size tier (Lucene
//!   `TieredMergePolicy`), swapped in by the owner's next manifest ([`SegmentLog`])
//!
//! Design: `docs/design/index-data-structures.md`, `docs/design/durability.md`
//!
//! [`DiskEngine`]: crate::DiskEngine

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
mod tests;

pub use emit::{describe_metrics, METRIC_BUCKETS};
pub(crate) use log::SegmentLog;
pub(crate) use meta::{decode_list, encode_list, SegmentMeta};
pub(crate) use reader::Snapshot;

use crate::pages::PageError;

/// Why a segment could not be read or written
#[derive(Debug, thiserror::Error)]
pub enum SegmentError {
    #[error("segment io: {0}")]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Page(#[from] PageError),

    #[error("segment {segment} filter build failed: {reason}")]
    Filter { segment: u32, reason: &'static str },
}

type Result<T> = std::result::Result<T, SegmentError>;

/// `<id:010>.seg`, zero-padded so a listing sorts by id
pub(crate) fn file_name(segment: u32) -> String {
    format!("{segment:010}.seg")
}

/// Id of a segment file or its checksums; `None` for anything else in the directory
fn parse_file_name(name: &str) -> Option<u32> {
    name.strip_suffix(".crc").unwrap_or(name).strip_suffix(".seg")?.parse().ok()
}
