#![doc = include_str!("../usage.md")]

use zaino_persistence::{IndexKind, Schema, Tables};
use zcash_protocol::consensus::NetworkType;

mod heights;
mod nodes;
mod reader;
mod serve;
mod subtrees;
mod tables;
mod writer;

pub use reader::TreeStateReader;
pub use serve::{pool_activations, ServeError};
pub use writer::{fold, FoldError, TreeStateIndexWriter};

/// On-disk layout version
const FORMAT: u16 = 1;

const TABLES: Tables = Tables::new(&tables::SEQUENCES, &[]);

/// Store layout on `network` (`zainod` opens and verifies by it)
pub const fn schema(network: NetworkType) -> Schema {
    Schema::new(IndexKind::TreeState, FORMAT, network, TABLES)
}

/// Buffered heap that commits a bulk run (a `FinalizedTip` block commits at once)
///
/// - every commit syncs each level's file (~50) → 8 MiB = commit-bound in sandblast
/// - 64 MiB = durable tip too far behind between commits
pub const WRITE_BUFFER: std::num::NonZeroUsize =
    std::num::NonZeroUsize::new(16 << 20).expect("16 MiB is non-zero");
