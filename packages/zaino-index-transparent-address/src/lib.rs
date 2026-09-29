//! Transparent address → received outputs, each with its spend (if any)
//!
//! # Data structure: two size-tiered LSM-Trees of sorted segments (`zaino_persistence::lsm`)
//!
//! ```text
//! <dir>/
//!   MANIFEST           committed count, tip hash, each set's segment list = the commit point
//!   receives/<id>.seg  one immutable sorted segment (+ `.crc`: page checksums)
//!   spent/<id>.seg     same, for spends
//!
//! receives row, 69 B:  addr 21 ([kind][hash160]) ‖ height u32 ‖ txid 32 ‖ vout u32 → value u64
//!                      sorted by address then height: one address = one contiguous range per segment
//!                      fences per 4 KiB block (≈59 rows), no filter (range-scanned, never probed)
//! spent row, 72 B:     txid 32 ‖ vout u32 → height u32 ‖ spending txid 32
//!                      fences per 4 KiB block (≈56 rows) + binary fuse filter (txid = uniform)
//!
//! integers big-endian: byte order = key order
//! ```
//!
//! - LSM minus everything mutable data needs: a spend = a new `spent` row, never a delete of its
//!   `receives` row → no memtable, no WAL, no tombstones, no versions
//! - no outpoint → address map, no UTXO set: `apply` = a pure projection of one block, no lookups
//! - `O(received)` per address, not `O(unspent)` (single-use receivers make the gap nil)
//! - memtable role = `NonFinalizedRows` (reorgable blocks, RAM only, keyed as the segments)
//!
//! # Lookup ([`TransparentAddressService::utxos`])
//!
//! ```text
//! address ──▶ receives:  nonfinalised rows in [from, tip]
//!                        + each segment: fences → block of (address, from) → walk rows while
//!                          the address matches                    (one contiguous range per segment)
//!                           │ merge, sort, dedupe by key
//!                           ▼
//! each received outpoint ──▶ spent:  nonfinalised map ──hit──▶ spent
//!                                       │ miss
//!                                       ▼
//!                                    each segment: filter ──"absent"──▶ next segment
//!                                       │ "maybe"                     (every segment but ≤ 1)
//!                                       ▼
//!                                    fences → one 4 KiB block → binary search ──found──▶ spent
//!                                                                 └──in no segment──▶ unspent
//! ```
//!
//! - balance = sum of the unspent; transactions = receiving txids + their spenders
//!
//! Policy and file format: `zaino_persistence::lsm`, `docs/design/index-data-structures.md` §5

mod address;
mod index_writer;
mod key;
mod serve;
mod view;

pub use index_writer::TransparentAddressIndexWriter;
pub use serve::{
    AddressUtxo, ServeError, TransactionRef, TransparentAddressService, DEFAULT_MAX_ADDRESS_ROWS,
};
pub use view::ReadView;

use std::{io, path::Path};

use zaino_persistence::{
    lsm::{self, LsmIndex, SegmentLog},
    manifest::IndexKind,
    pages::CommittedFiles,
};
use zcash_protocol::consensus::NetworkType;

use key::{ReceiveRow, SpentRow};

/// Every file `dir`'s manifest seals (offline scrub; plain reads, no lock)
pub fn committed_files(dir: &Path, network: NetworkType) -> io::Result<CommittedFiles> {
    lsm::committed_files::<TransparentAddressIndex>(dir, network)
}

/// On disk: a `receives` and a `spent` segment set
struct TransparentAddressIndex;

impl LsmIndex for TransparentAddressIndex {
    const KIND: IndexKind = IndexKind::TransparentAddress;
    const FORMAT: u16 = 1;
    const SETS: &'static [&'static str] = &["receives", "spent"];
    type Logs = (SegmentLog<ReceiveRow>, SegmentLog<SpentRow>);
}
