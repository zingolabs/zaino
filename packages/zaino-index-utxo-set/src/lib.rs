#![doc = include_str!("../usage.md")]

mod error;
mod layout;
mod reader;
mod writer;

pub use error::FoldError;
pub use reader::UtxoSetReader;
pub use writer::{fold, UtxoSetIndexWriter};

use zaino_persistence::{IndexKind, MapTable, Schema, SequenceTable, Tables, Width};
use zcash_protocol::consensus::NetworkType;

use layout::{OUTPOINT, VALUE};

/// On-disk layout version
const FORMAT: u16 = 1;

const TABLES: Tables = Tables::new(&[FEES], &[OUTPUTS]);

/// Store layout on `network` (`zainod` opens and verifies by it)
pub const fn schema(network: NetworkType) -> Schema {
    Schema::new(IndexKind::UtxoSet, FORMAT, network, TABLES)
}

/// Buffered heap that commits a bulk run (a `FinalizedTip` block commits at once)
pub const WRITE_BUFFER: std::num::NonZeroUsize =
    std::num::NonZeroUsize::new(256 << 20).expect("256 MiB is non-zero");

const OUTPUTS: MapTable =
    MapTable::point(0, "outputs", OUTPOINT as u32, VALUE as u32).cache_writes().deletes();
const FEES: SequenceTable = SequenceTable::new(0, "fees", Width::Variable);
