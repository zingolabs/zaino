//! Transparent outpoint → value; block → per-tx [`Fee`](zaino_primitives::types::Fee) for the
//! [`FeeSink`](zaino_sync::FeeSink)
//!
//! # Data structure: size-tiered LSM-Tree of immutable sorted segments (`zaino_persistence::lsm`)
//!
//! ```text
//! <dir>/
//!   MANIFEST           committed count, tip hash, segment list (id, rows, seal) = the commit point
//!   outputs/<id>.seg   one immutable sorted segment (+ `.crc`: page checksums)
//!
//! one segment file:
//!   rows      txid 32 ‖ vout u32 BE → value u64 BE, 44 B each, sorted by outpoint, packed
//!   fences    first outpoint of every 4 KiB block of rows (≈93 rows per block)
//!   filter    binary fuse, 8-bit fingerprints (txid = uniform)
//! ```
//!
//! - LSM minus everything mutable data needs: spent outputs kept, never deleted → no memtable, no
//!   WAL, no tombstones (any height re-resolves identically: a downstream index behind this one
//!   replays through delivery, no rewind)
//! - memtable role = `pending::Pending` (every output delivered above the durable tip, staged and
//!   non-finalized alike, RAM only)
//! - one item published per block at delivery (bulk, replay and tip alike)
//!
//! # Lookup (`index_writer::resolve`, per transparent input)
//!
//! ```text
//! prevout ──▶ pending map ──hit──▶ value            (this block's own outputs included)
//!    │ miss
//!    ▼
//! each segment:  filter ──"absent"──▶ next segment      (every segment but ≤ 1)
//!                  │ "maybe"
//!                  ▼
//!                fences → one 4 KiB block → binary search ──found──▶ value
//!                                                  └──in no segment──▶ `MissingPrevout` (fatal)
//! ```
//!
//! Policy and file format: `zaino_persistence::lsm`, `docs/design/index-data-structures.md` §5

mod index_writer;
mod key;
mod pending;

pub use index_writer::{IndexWriterError, ValueBalanceIndexWriter};

use std::{io, path::Path};

use zaino_persistence::{
    lsm::{self, LsmIndex, SegmentLog},
    manifest::IndexKind,
    pages::CommittedFiles,
};
use zcash_protocol::consensus::NetworkType;

use key::OutputRow;

/// Every file `dir`'s manifest seals (offline scrub; plain reads, no lock)
pub fn committed_files(dir: &Path, network: NetworkType) -> io::Result<CommittedFiles> {
    lsm::committed_files::<ValueBalanceIndex>(dir, network)
}

/// On disk: one `outputs` segment set
struct ValueBalanceIndex;

impl LsmIndex for ValueBalanceIndex {
    const KIND: IndexKind = IndexKind::ValueBalance;
    const FORMAT: u16 = 1;
    const SETS: &'static [&'static str] = &["outputs"];
    type Logs = SegmentLog<OutputRow>;
}
