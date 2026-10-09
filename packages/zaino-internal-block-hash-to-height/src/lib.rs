//! Block hash → height
//!
//! # Data structure: one point-lookup map on the persistence port (`zaino_persistence`)
//!
//! ```text
//! by_hash   hash 32 (protocol byte order) → height u32 BE      scope 0: read whole, never scanned
//! ```
//!
//! - insert only: a finalised block's height never changes (no tombstones, no versions)
//! - one block = one row ([`fold`]); non-final blocks = `zaino-nfs` layers, read through a
//!   snapshot's `OverlayView`
//! - segments, merges, filters, manifest, crash safety = the engine's

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

/// Buffered heap that commits a bulk run (≈100 B per block → ≈20k blocks; a `Finalized` block
/// commits at once)
pub const WRITE_BUFFER: std::num::NonZeroUsize =
    std::num::NonZeroUsize::new(2 << 20).expect("2 MiB is non-zero");
