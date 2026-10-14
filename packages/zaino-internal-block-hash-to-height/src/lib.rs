#![doc = include_str!("../usage.md")]

use zaino_persistence::Tables;

use by_hash::BY_HASH;

mod by_hash;
mod reader;
mod writer;

pub use reader::BlockHashReader;
pub use writer::{fold, BlockHashIndexWriter};

/// Block hash width
pub(crate) const HASH: usize = 32;

/// On-disk layout version
pub const FORMAT: u16 = 1;

/// What the store holds (`zainod` opens and verifies it by these)
pub const TABLES: Tables = Tables::new(&[], &[BY_HASH]);

/// Buffered heap that commits a bulk run (≈100 B per block → ≈20k blocks; a `FinalizedTip` block
/// commits at once)
pub const WRITE_BUFFER: std::num::NonZeroUsize =
    std::num::NonZeroUsize::new(2 << 20).expect("2 MiB is non-zero");
