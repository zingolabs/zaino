//! Height → gRPC-framed `CompactBlock` record, one `Variable` sequence through the persistence port
//!
//! ```text
//! <dir>/                                    e.g. `zaino_persistence::DiskEngine`
//!   MANIFEST      committed tip + each file's seal = the commit point
//!   blocks.dat    framed records, back to back, height order
//!   blocks.idx    end offset per record (record h = height h)
//!
//! one record:     [0x00][len u32 BE][CompactBlock protobuf] = the exact bytes on the wire
//!                 every pool included (pruned on read, `project.rs`)
//! ```
//!
//! - records = wire bytes: serving = byte movement (no decode, no re-encode)
//! - tree sizes after the tip = the tip record's `chainMetadata` (read at open and after a reorg)
//! - blocks above the durable tip = `zaino_persistence::Tiered` (same bytes, RAM only)
//! - hash → height = `zaino-internal-block-hash-to-height` (record's own `hash` confirms a hit)
//!
//! # Lookup (`ReadView`)
//!
//! ```text
//! height h ──▶ held records ──hit──▶ record
//!    │ miss
//!    ▼
//! blocks[h]  (a slice of the mapping)
//!
//! heights a..=b ──▶ <= SPAN_RECORDS records from a, cut to the cursor's byte budget (>= 1)
//! ```

use zaino_persistence::{IndexKind, Schema, SequenceId, SequenceRead, Width};
use zaino_primitives::types::{Height, TreeSizes};
use zcash_protocol::consensus::NetworkType;

mod build;
mod index_writer;
mod project;
mod serve;
mod view;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use build::{compact_tx, encode_compact_block};
pub use index_writer::CompactBlockIndexWriter;
pub use project::{project_tx_at, Pools};
pub use serve::{CompactBlockService, RangeCursor, ServeError};
pub use view::ReadView;

/// On-disk layout version
const FORMAT: u16 = 1;

pub(crate) const BLOCKS: SequenceId = SequenceId(0);

/// Block hash width (`CompactBlock.hash`)
pub(crate) const HASH: usize = 32;

/// What a store holds for this index (opened and verified by it)
pub fn schema(network: NetworkType) -> Schema {
    Schema::new(IndexKind::CompactBlock, FORMAT, network).with_sequence(
        BLOCKS,
        "blocks",
        Width::Variable,
    )
}

/// `BLOCKS` position of `height`
pub(crate) fn position(height: Height) -> u64 {
    u64::from(u32::from(height))
}

/// Tree sizes after `view`'s tip (its record's `chainMetadata`; nothing held = zero)
pub(crate) fn tip_sizes(view: &impl SequenceRead) -> TreeSizes {
    let Some(tip) = view.tip() else { return TreeSizes::ZERO };
    let record = view.record(BLOCKS, position(tip.height));
    let record = record.unwrap_or_else(|| panic!("compact_block: no record at its tip {tip:?}"));
    project::record_sizes(&record).expect("every record carries its chainMetadata")
}
