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
//!   snapshot's `LayeredView`
//! - segments, merges, filters, manifest, crash safety = the engine's

use zaino_persistence::{IndexKind, Schema, Width};
use zcash_protocol::consensus::NetworkType;

use by_hash::{BY_HASH, HEIGHT};

mod by_hash;
mod fold;
mod reader;
mod writer;

pub use fold::fold;
pub use reader::BlockHashReader;
pub use writer::BlockHashIndexWriter;

/// Block hash width
pub(crate) const HASH: usize = 32;

/// On-disk layout version
const FORMAT: u16 = 1;

/// What the store holds (`zainod verify` reads by it)
pub fn schema(network: NetworkType) -> Schema {
    let (key, value) = (Width::fixed(HASH as u32), Width::fixed(HEIGHT as u32));
    Schema::new(IndexKind::BlockHash, FORMAT, network).with_map(BY_HASH, "by_hash", key, value, 0)
}
