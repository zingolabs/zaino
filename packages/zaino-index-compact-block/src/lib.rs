#![doc = include_str!("../usage.md")]

use zaino_persistence::{IndexKind, Schema, SequenceTable, Tables, Width};
use zcash_protocol::consensus::{BlockHeight, NetworkType};

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
const FORMAT: u16 = 1;

const TABLES: Tables = Tables::new(&[BLOCKS], &[]);

/// Store layout on `network` (`zainod` opens and verifies by it)
pub const fn schema(network: NetworkType) -> Schema {
    Schema::new(IndexKind::CompactBlock, FORMAT, network, TABLES)
}

/// Buffered heap that commits a bulk run (spam-era ≈0.5 MB records → ≈250 blocks; a `FinalizedTip`
/// block commits at once)
pub const WRITE_BUFFER: std::num::NonZeroUsize =
    std::num::NonZeroUsize::new(128 << 20).expect("128 MiB is non-zero");

pub(crate) const BLOCKS: SequenceTable = SequenceTable::new(0, "blocks", Width::Variable);

/// Block hash width (`CompactBlock.hash`)
pub(crate) const HASH: usize = 32;

/// `BLOCKS` position of `height`
pub(crate) fn position(height: BlockHeight) -> u64 {
    u64::from(u32::from(height))
}
