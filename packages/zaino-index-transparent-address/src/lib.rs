#![doc = include_str!("../usage.md")]

mod address;
mod layout;
mod reader;
mod serve;
mod writer;

pub use reader::TransparentAddressReader;
pub use serve::{AddressUtxo, ServeError, TransactionRef, DEFAULT_MAX_ADDRESS_ROWS};
pub use writer::{fold, TransparentAddressIndexWriter};

use zaino_persistence::{IndexKind, MapTable, Schema, Tables};
use zcash_protocol::consensus::NetworkType;

use address::AddressKey;
use layout::{RECEIVE_KEY, RECEIVE_VALUE, SPEND, SPENT_KEY};

/// On-disk layout version
const FORMAT: u16 = 1;

const TABLES: Tables = Tables::new(&[], &[RECEIVES, SPENT]);

/// Store layout on `network` (`zainod` opens and verifies by it)
pub const fn schema(network: NetworkType) -> Schema {
    Schema::new(IndexKind::TransparentAddress, FORMAT, network, TABLES)
}

/// Buffered heap that commits a bulk run (a `FinalizedTip` block commits at once)
pub const WRITE_BUFFER: std::num::NonZeroUsize =
    std::num::NonZeroUsize::new(256 << 20).expect("256 MiB is non-zero");

const RECEIVES: MapTable = MapTable::scoped(
    0,
    "receives",
    RECEIVE_KEY as u32,
    RECEIVE_VALUE as u32,
    AddressKey::LEN as u32,
);
const SPENT: MapTable = MapTable::point(1, "spent", SPENT_KEY as u32, SPEND as u32);
