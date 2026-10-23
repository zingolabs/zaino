#![doc = include_str!("../usage.md")]

use zaino_persistence::{IndexKind, Schema, Tables};
use zcash_protocol::consensus::NetworkType;

use by_hash::BY_HASH;

mod by_hash;
mod reader;
mod writer;

pub use reader::BlockHashReader;
pub use writer::{fold, BlockHashIndexWriter};

/// Block hash width
pub(crate) const HASH: usize = 32;

const FORMAT: u16 = 1;

const TABLES: Tables = Tables::new(&[], &[BY_HASH]);

/// Store layout on `network` (`zainod` opens and verifies by it)
pub const fn schema(network: NetworkType) -> Schema {
    Schema::new(IndexKind::BlockHash, FORMAT, network, TABLES)
}

/// Buffered heap that commits a bulk run (≈100 B per block → ≈20k blocks)
///
/// - `FinalizedTip` block commits at once
pub const WRITE_BUFFER: std::num::NonZeroUsize =
    std::num::NonZeroUsize::new(2 << 20).expect("2 MiB is non-zero");
