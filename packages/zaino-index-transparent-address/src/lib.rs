//! Transparent address → received outputs, each with its spend (if any)
//!
//! # Data structure: two maps in one store (`zaino_persistence`, [`schema`])
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
//! - blocks above the durable tip = `zaino_persistence::Tiered` (RAM only, keyed as the maps)
//!
//! # Lookup ([`TransparentAddressReader`], through [`TransparentAddressService::utxos`])
//!
//! ```text
//! address ──▶ receives:  `range(RECEIVES, (addr, start), (addr, tip + 1), budget)`
//!                        (held rows merged over committed ones, by key)
//!                           │
//!                           ▼
//! each received outpoint ──▶ spent:  one `values(SPENT, outpoints)` ──found──▶ spent
//!                                    (held first, then committed)   └──absent──▶ unspent
//! ```
//!
//! - balance = sum of the unspent; transactions = receiving txids + their spenders
//! - `docs/design/index-data-structures.md` §5

mod address;
mod fold;
mod key;
mod reader;
mod serve;
mod writer;

pub use fold::fold;
pub use reader::TransparentAddressReader;
pub use serve::{
    AddressUtxo, ServeError, TransactionRef, TransparentAddressService, DEFAULT_MAX_ADDRESS_ROWS,
};
pub use writer::TransparentAddressIndexWriter;

use zaino_persistence::{IndexKind, MapId, Schema, Width};
use zcash_protocol::consensus::NetworkType;

use key::{AddressKey, RECEIVE_KEY, RECEIVE_VALUE, SPEND};
use zaino_primitives::types::OutPoint;

/// On-disk layout version
const FORMAT: u16 = 1;
const RECEIVES: MapId = MapId(0);
const SPENT: MapId = MapId(1);

/// What the index stores (`zainod verify` scrubs a directory against it)
pub fn schema(network: NetworkType) -> Schema {
    let width = |bytes: usize| Width::fixed(bytes as u32);
    Schema::new(IndexKind::TransparentAddress, FORMAT, network)
        .with_map(
            RECEIVES,
            "receives",
            width(RECEIVE_KEY),
            width(RECEIVE_VALUE),
            AddressKey::LEN as u32,
        )
        .with_map(SPENT, "spent", width(OutPoint::LEN), width(SPEND), 0)
}
