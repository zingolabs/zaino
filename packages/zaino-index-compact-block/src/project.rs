//! Pool pruning + single-field reads by walking protobuf framing (no decode)
//!
//! - record stored with every pool + every tx; `BlockRange.poolTypes` asks for a subset
//! - tx left with no pool component dropped, every selection (lightwalletd `FilterTxPool`)
//! - copies retained spans, skips dropped ones: each byte touched at most once (a decode +
//!   re-encode = a `Vec` per field, every struct rebuilt)
//! - two nesting levels only: the block's `vtx` entries, the per-pool fields inside each tx
//! - everything else copied verbatim (an unknown field survives, never silently dropped)

use bytes::Bytes;
use prost::Message as _;
use zaino_primitives::types::{TreeSize, TreeSizes};
use zaino_proto::frame::{frame_into, FRAME_HEADER};
use zaino_proto::proto::compact_formats as cf;

use crate::HASH;

/// `CompactBlock.vtx`, the only block-level field rewritten
///
/// - numbers read off `compact_formats.proto` (field 1 = `protoVersion`, reserved upstream)
/// - projection tests decode with `prost` (a numbering drift fails, never serves wrong bytes)
const BLOCK_VTX: u64 = 7;

const BLOCK_HASH: u64 = 3;
const BLOCK_CHAIN_METADATA: u64 = 8;

/// `CompactTx.index` (varint: wire type 0, so the key = the field number shifted)
const TX_INDEX: u64 = 1;

/// Per-pool `CompactTx` fields
const TX_SAPLING_SPENDS: u64 = 4;
const TX_SAPLING_OUTPUTS: u64 = 5;
const TX_ORCHARD_ACTIONS: u64 = 6;
const TX_VIN: u64 = 7;
const TX_VOUT: u64 = 8;
const TX_IRONWOOD_ACTIONS: u64 = 9;

/// Pools a response carries ([`Default`] = empty `poolTypes` = every shielded pool, no
/// transparent)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pools {
    pub sapling: bool,
    pub orchard: bool,
    pub ironwood: bool,
    pub transparent: bool,
}

impl Default for Pools {
    fn default() -> Self {
        Self { sapling: true, orchard: true, ironwood: true, transparent: false }
    }
}

impl Pools {
    /// Every pool (= what the record holds: projection a no-op)
    pub const ALL: Self = Self { sapling: true, orchard: true, ironwood: true, transparent: true };

    /// `Some(kept)` for a pool component field, `None` for the rest (identity, fee: always kept)
    fn component(&self, field: u64) -> Option<bool> {
        match field {
            TX_SAPLING_SPENDS | TX_SAPLING_OUTPUTS => Some(self.sapling),
            TX_ORCHARD_ACTIONS => Some(self.orchard),
            TX_IRONWOOD_ACTIONS => Some(self.ironwood),
            TX_VIN | TX_VOUT => Some(self.transparent),
            _ => None,
        }
    }
}

/// Framed records, in the order given, back to back in one buffer, each rewritten to carry only
/// `pools`
///
/// - `None` on a malformed record, never a partial span (a record that will not walk = corruption)
pub(crate) fn project(records: &[Bytes], pools: Pools) -> Option<Bytes> {
    let mut out = Vec::with_capacity(records.iter().map(Bytes::len).sum());
    for record in records {
        let block = record.get(FRAME_HEADER..)?;
        frame_into(&mut out, |out| project_block(block, pools, out))?;
    }
    Some(Bytes::from(out))
}

/// One mempool `CompactTx` (encoded at slot 0: `index` absent) at `slot`, carrying only `pools`,
/// framed; `None` = no component left (dropped, as from a block)
///
/// - `encoded` = this process's own render: unwalkable = a bug, panics
pub fn project_tx_at(encoded: &[u8], slot: u64, pools: Pools) -> Option<Bytes> {
    let mut out = Vec::with_capacity(FRAME_HEADER + encoded.len() + 11);
    let kept = frame_into(&mut out, |out| {
        put_varint(TX_INDEX << 3, out);
        put_varint(slot, out);
        project_tx(encoded, pools, out).expect("a rendered CompactTx walks")
    });
    kept.then(|| Bytes::from(out))
}

/// One framed record's `CompactBlock.hash` (`None` = absent, not 32 bytes, or unwalkable)
///
/// - precedes `vtx`: a lookup never walks the transactions
pub(crate) fn record_hash(record: &[u8]) -> Option<[u8; HASH]> {
    block_field(record, BLOCK_HASH)?.try_into().ok()
}

/// One framed record's `CompactBlock.chainMetadata` (`None` = absent or unwalkable)
///
/// - follows `vtx`: each tx skipped by its length, never decoded
pub(crate) fn record_sizes(record: &[u8]) -> Option<TreeSizes> {
    let metadata = cf::ChainMetadata::decode(block_field(record, BLOCK_CHAIN_METADATA)?).ok()?;
    Some(TreeSizes {
        sapling: TreeSize::from(metadata.sapling_commitment_tree_size),
        orchard: TreeSize::from(metadata.orchard_commitment_tree_size),
        ironwood: TreeSize::from(metadata.ironwood_commitment_tree_size),
    })
}

/// Value of the first `field` of one framed record's `CompactBlock` (stops there)
fn block_field(record: &[u8], field: u64) -> Option<&[u8]> {
    let block = record.get(FRAME_HEADER..)?;
    let mut cursor = 0usize;
    while cursor < block.len() {
        let (key, next) = varint(block, cursor)?;
        let (value, after) = field_value(block, next, key)?;
        if key >> 3 == field {
            return Some(value);
        }
        cursor = after;
    }
    None
}

/// `CompactBlock` copied, each `vtx` entry rewritten (dropped once no component is left)
fn project_block(block: &[u8], pools: Pools, out: &mut Vec<u8>) -> Option<()> {
    let mut cursor = 0usize;

    while cursor < block.len() {
        let (key, next) = varint(block, cursor)?;
        let (value, after) = field_value(block, next, key)?;

        if key >> 3 == BLOCK_VTX {
            let mut tx = Vec::with_capacity(value.len());
            if project_tx(value, pools, &mut tx)? {
                put_varint(key, out);
                put_varint(tx.len() as u64, out);
                out.extend_from_slice(&tx);
            }
        } else {
            copy_field(block, cursor, after, out)?;
        }

        cursor = after;
    }

    Some(())
}

/// `CompactTx` copied, fields of unrequested pools dropped; `true` = >= 1 component kept
/// (repeated field on the wire = non-empty)
fn project_tx(tx: &[u8], pools: Pools, out: &mut Vec<u8>) -> Option<bool> {
    let mut cursor = 0usize;
    let mut has_component = false;

    while cursor < tx.len() {
        let (key, next) = varint(tx, cursor)?;
        let (_, after) = field_value(tx, next, key)?;

        let component = pools.component(key >> 3);
        has_component |= component == Some(true);
        if component != Some(false) {
            copy_field(tx, cursor, after, out)?;
        }

        cursor = after;
    }

    Some(has_component)
}

/// One whole field (key + value) copied verbatim: bytes `start` inclusive to `end` exclusive
fn copy_field(bytes: &[u8], start: usize, end: usize, out: &mut Vec<u8>) -> Option<()> {
    out.extend_from_slice(bytes.get(start..end)?);
    Some(())
}

/// `(value bytes of the field keyed `key`, offset just past it)`
///
/// - wire types 3, 4 (groups) = proto2-only, absent from this schema → unwalkable (walk total)
fn field_value(bytes: &[u8], at: usize, key: u64) -> Option<(&[u8], usize)> {
    match key & 0b111 {
        // varint
        0 => {
            let (_, after) = varint(bytes, at)?;
            Some((&[], after))
        }
        // 64-bit
        1 => Some((&[], at.checked_add(8).filter(|end| *end <= bytes.len())?)),
        // length-delimited
        2 => {
            let (len, start) = varint(bytes, at)?;
            let end = start.checked_add(usize::try_from(len).ok()?)?;
            Some((bytes.get(start..end)?, end))
        }
        // 32-bit
        5 => Some((&[], at.checked_add(4).filter(|end| *end <= bytes.len())?)),
        _ => None,
    }
}

/// `(base-128 varint, offset just past it)`
fn varint(bytes: &[u8], at: usize) -> Option<(u64, usize)> {
    let mut value = 0u64;
    let mut shift = 0u32;
    let mut cursor = at;

    loop {
        let byte = *bytes.get(cursor)?;
        cursor += 1;

        value |= u64::from(byte & 0x7f).checked_shl(shift)?;
        if byte & 0x80 == 0 {
            return Some((value, cursor));
        }

        shift += 7;
        // protobuf varint ≤ 10 bytes (beyond = malformed record)
        if shift >= 64 {
            return None;
        }
    }
}

fn put_varint(mut value: u64, out: &mut Vec<u8>) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}
