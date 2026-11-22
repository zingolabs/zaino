#![doc = include_str!("../usage.md")]

use zaino_persistence::{Schema, SequenceTable, Tables, Width};
use zaino_sync::IndexKind;
use zcash_protocol::consensus::NetworkType;

mod encode;
mod project;
mod reader;
mod serve;
mod writer;

pub use encode::{compact_tx, encode_compact_block};
pub use project::{project_tx_at, Pools};
pub use reader::CompactBlockReader;
pub use serve::{RangeCursor, ServeError};
pub use writer::{fold, CompactBlockIndexWriter};

const FORMAT: u16 = 1;

const TABLES: Tables = Tables::new(&[BLOCKS], &[]);

/// Store layout on `network` (`zainod` opens and verifies by it)
pub const fn schema(network: NetworkType) -> Schema {
    zaino_sync::schema(IndexKind::CompactBlock, FORMAT, network, TABLES)
}

/// Buffered heap that commits a bulk run (spam-era ≈0.5 MB records → ≈250 blocks)
///
/// - `FinalizedTip` block commits at once
pub const WRITE_BUFFER: std::num::NonZeroUsize =
    std::num::NonZeroUsize::new(128 << 20).expect("128 MiB is non-zero");

pub(crate) const BLOCKS: SequenceTable = SequenceTable::new(0, "blocks", Width::Variable);
