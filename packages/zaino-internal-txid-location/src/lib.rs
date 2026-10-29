#![doc = include_str!("../usage.md")]

use zaino_persistence::{Schema, Tables};
use zaino_sync::IndexKind;
use zcash_protocol::consensus::NetworkType;

use by_txid::BY_TXID;

mod by_txid;
mod reader;
mod writer;

pub use by_txid::Mined;
pub use reader::TxidLocationReader;
pub use writer::{fold, TxidLocationIndexWriter};

const FORMAT: u16 = 1;

const TABLES: Tables = Tables::new(&[], &[BY_TXID]);

/// Store layout on `network` (`zainod` opens and verifies by it)
pub const fn schema(network: NetworkType) -> Schema {
    zaino_sync::schema(IndexKind::TxidLocation, FORMAT, network, TABLES)
}

/// Buffered heap that commits a bulk run (≈120 B per transaction)
///
/// - `FinalizedTip` block commits at once
pub const WRITE_BUFFER: std::num::NonZeroUsize =
    std::num::NonZeroUsize::new(16 << 20).expect("16 MiB is non-zero");
