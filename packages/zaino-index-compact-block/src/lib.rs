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
//! - tree sizes after a block = its record's `chainMetadata` ([`fold`] reads the parent's)
//! - hash → height = `zaino-internal-block-hash-to-height` (record's own `hash` confirms a hit)

use zaino_persistence::{SequenceTable, Tables, Width};
use zaino_primitives::types::Height;

mod build;
mod project;
mod reader;
mod serve;
mod writer;

pub use build::{compact_tx, encode_compact_block};
pub use project::{project_tx_at, Pools};
pub use reader::CompactBlockReader;
pub use serve::{RangeCursor, ServeError};
pub use writer::{fold, CompactBlockIndexWriter};

/// On-disk layout version
pub const FORMAT: u16 = 1;

/// What the store holds (`zainod` opens and verifies it by these)
pub const TABLES: Tables = Tables::new(&[BLOCKS], &[]);

pub(crate) const BLOCKS: SequenceTable = SequenceTable::new(0, "blocks", Width::Variable);

/// Block hash width (`CompactBlock.hash`)
pub(crate) const HASH: usize = 32;

/// `BLOCKS` position of `height`
pub(crate) fn position(height: Height) -> u64 {
    u64::from(u32::from(height))
}
