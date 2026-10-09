//! Transparent address → received outputs, each with its spend (if any)
//!
//! # Data structure: two maps in one store (`zaino_persistence`, [`TABLES`])
//!
//! ```text
//! receives  addr 21 ([hash160][kind]) ‖ height u32 ‖ txid 32 ‖ vout u32 → value u64
//!           scope = addr: one address's history = one key range
//! spent     txid 32 ‖ vout u32 (OutPoint::encode) → height u32 ‖ spending txid 32
//!           point lookups only
//!
//! integers big-endian: byte order = key order
//! ```
//!
//! - insert only: a spend = a new `spent` row, never a delete of its `receives` row
//! - no outpoint → address map, no UTXO set: [`fold`] = a pure projection of one block, no lookups
//! - `O(received)` per address, not `O(unspent)` (single-use receivers make the gap nil)
//! - non-final blocks = `zaino-nfs` layers (keyed as the maps), read through an `OverlayView`
//!
//! # Lookup ([`TransparentAddressReader::utxos_of`], as of the served tip)
//!
//! ```text
//! address ──▶ receives:  `map(RECEIVES).range((addr, start), (addr, tip + 1), budget)`
//!                        (layer rows merged over committed ones, by key)
//!                           │
//!                           ▼
//! each received outpoint ──▶ spent:  one `map(SPENT).values(outpoints)` ──found ≤ tip──▶ spent
//!                                    (layer first, then committed)     └──otherwise──▶ unspent
//! ```
//!
//! - balance = sum of the unspent; transactions = receiving txids + their spenders
//! - `docs/design/index-data-structures.md` §5

mod address;
mod key;
mod reader;
mod serve;
mod writer;

pub use reader::TransparentAddressReader;
pub use serve::{AddressUtxo, ServeError, TransactionRef, DEFAULT_MAX_ADDRESS_ROWS};
pub use writer::{fold, TransparentAddressIndexWriter};

use zaino_persistence::{MapTable, Tables, Width};
use zaino_primitives::types::OutPoint;

use key::{AddressKey, RECEIVE_KEY, RECEIVE_VALUE, SPEND};

/// On-disk layout version
pub const FORMAT: u16 = 1;

/// What the store holds (`zainod` opens and verifies it by these)
pub const TABLES: Tables = Tables::new(&[], &[RECEIVES, SPENT]);

/// Buffered heap that commits a bulk run (a `Finalized` block commits at once)
pub const WRITE_BUFFER: std::num::NonZeroUsize =
    std::num::NonZeroUsize::new(256 << 20).expect("256 MiB is non-zero");

const RECEIVES: MapTable = MapTable::new(
    0,
    "receives",
    Width::fixed(RECEIVE_KEY as u32),
    Width::fixed(RECEIVE_VALUE as u32),
    AddressKey::LEN as u32,
);
const SPENT: MapTable =
    MapTable::new(1, "spent", Width::fixed(OutPoint::LEN as u32), Width::fixed(SPEND as u32), 0);
