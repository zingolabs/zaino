#![doc = include_str!("../usage.md")]

mod address;
mod layout;
mod reader;
mod serve;
mod writer;

pub use reader::TransparentAddressReader;
pub use serve::{AddressUtxo, ServeError, DEFAULT_MAX_ADDRESS_ROWS};
pub use writer::{fold, TransparentAddressIndexWriter};

use zaino_persistence::{MapTable, Schema, Tables};
use zaino_sync::IndexKind;
use zcash_protocol::consensus::NetworkType;

use address::AddressKey;
use layout::{RECEIVE_KEY, RECEIVE_VALUE, SPEND, SPENT_KEY};

const FORMAT: u16 = 1;

const TABLES: Tables = Tables::new(&[], &[RECEIVES, SPENT]);

/// Store layout on `network` (`zainod` opens and verifies by it)
pub const fn schema(network: NetworkType) -> Schema {
    zaino_sync::schema(IndexKind::TransparentAddress, FORMAT, network, TABLES)
}

/// Buffered heap that commits a bulk run (a `FinalizedTip` block commits at once)
pub const WRITE_BUFFER: std::num::NonZeroUsize =
    std::num::NonZeroUsize::new(256 << 20).expect("256 MiB is non-zero");

const RECEIVES: MapTable =
    MapTable::scoped(0, "receives", RECEIVE_KEY, RECEIVE_VALUE, AddressKey::LEN);
const SPENT: MapTable = MapTable::point(1, "spent", SPENT_KEY, SPEND);
