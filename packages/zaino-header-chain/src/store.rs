//! Final headers on disk: one sequence, record `h` = the header at height `h`
//!
//! - written only once verified (from genesis): reopen resumes, never re-verifies, never a
//!   checkpoint a validator supplied

use std::{path::Path, sync::Arc};

use zaino_persistence::{
    fs::Fs, Changes, DiskEngine, DiskStore, DiskView, IndexKind, PersistenceEngine, Schema,
    SequenceId, SequenceRead, Store, StoreError, View, Width,
};
use zaino_primitives::types::{BlockHash, BlockRef, Height, MerkleRoot};
use zcash_protocol::consensus::NetworkType;

/// On-disk layout version
const FORMAT: u16 = 1;
const HEADERS: SequenceId = SequenceId(0);
pub(crate) const RECORD: usize = 88;

pub fn schema(network: NetworkType) -> Schema {
    Schema::new(IndexKind::HeaderChain, FORMAT, network).with_sequence(
        HEADERS,
        "headers",
        Width::fixed(RECORD as u32),
    )
}

/// One header as the chain keeps it: identity, the fields later rules and block checks read, and
/// the work of everything up to it
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record {
    pub hash: BlockHash,
    pub merkle_root: MerkleRoot,
    pub time: u32,
    pub bits: u32,
    pub cumulative_work: u128,
}

/// hash 32 · merkle root 32 · time u32 LE · bits u32 LE · cumulative work u128 LE
pub(crate) fn encode(record: &Record) -> [u8; RECORD] {
    let mut out = [0u8; RECORD];
    out[..32].copy_from_slice(&<[u8; 32]>::from(record.hash));
    out[32..64].copy_from_slice(&<[u8; 32]>::from(record.merkle_root));
    out[64..68].copy_from_slice(&record.time.to_le_bytes());
    out[68..72].copy_from_slice(&record.bits.to_le_bytes());
    out[72..].copy_from_slice(&record.cumulative_work.to_le_bytes());
    out
}

pub(crate) fn decode(bytes: &[u8; RECORD]) -> Record {
    let hash = |at: usize| -> [u8; 32] { bytes[at..at + 32].try_into().expect("fixed field") };
    let word = |at: usize| -> [u8; 4] { bytes[at..at + 4].try_into().expect("fixed field") };
    Record {
        hash: BlockHash::from(hash(0)),
        merkle_root: MerkleRoot::from(hash(32)),
        time: u32::from_le_bytes(word(64)),
        bits: u32::from_le_bytes(word(68)),
        cumulative_work: u128::from_le_bytes(bytes[72..].try_into().expect("fixed field")),
    }
}

/// Append-only, single-writer store of final headers
#[derive(Debug)]
pub struct HeaderStore {
    store: DiskStore,
    view: HeaderView,
}

/// One committed state of the store: never changes, whatever is appended after it
#[derive(Debug, Clone)]
pub(crate) struct HeaderView {
    view: DiskView,
}

impl HeaderView {
    pub(crate) fn tip(&self) -> Option<BlockRef> {
        self.view.tip()
    }

    /// Records `from..=to` (heights committed), oldest first
    pub(crate) fn records(&self, from: Height, to: Height) -> Vec<Record> {
        let range = u64::from(u32::from(from))..u64::from(u32::from(to)) + 1;
        let records = self.view.records(HEADERS, range);
        records.iter().map(|bytes| decode(bytes[..].try_into().expect("RECORD bytes"))).collect()
    }

    pub(crate) fn record(&self, at: Height) -> Option<Record> {
        self.records(at, at).first().copied()
    }
}

impl HeaderStore {
    /// `path` at its committed state; fresh = empty
    pub fn open(fs: Arc<dyn Fs>, path: &Path, network: NetworkType) -> Result<Self, StoreError> {
        let store = DiskEngine::new(fs).open(path, &schema(network))?;
        let view = store.view();
        let held = view.tip().map_or(0, |tip| u64::from(u32::from(tip.height)) + 1);
        assert_eq!(view.len(HEADERS), held, "header store: one record per committed height");
        Ok(Self { store, view: HeaderView { view } })
    }

    /// Last final header (`None` = nothing final yet)
    pub fn tip(&self) -> Option<BlockRef> {
        self.view.tip()
    }

    pub(crate) fn view(&self) -> HeaderView {
        self.view.clone()
    }

    /// Appends `records` from the next height on, commits (durable on return)
    pub(crate) fn append(&mut self, records: &[(Height, Record)]) -> Result<(), StoreError> {
        let Some(&(last_height, last)) = records.last() else { return Ok(()) };
        let next = self.tip().map_or(Height::GENESIS, |tip| tip.height.next());
        assert_eq!(records[0].0, next, "final records off the committed tip");
        let tip = BlockRef { hash: last.hash, height: last_height };
        let mut changes = Changes::new(tip, self.store.schema());
        for (_, record) in records {
            changes.append(HEADERS, &encode(record));
        }
        self.store.apply(changes);
        self.store.commit()?;
        self.view = HeaderView { view: self.store.view() };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Field order and endianness pinned; decode inverts encode
    #[test]
    fn a_record_is_its_golden_bytes() {
        let mut hash = [0u8; 32];
        hash[0] = 0xab;
        hash[31] = 0x01;
        let record = Record {
            hash: BlockHash::from(hash),
            merkle_root: MerkleRoot::from([0x5a; 32]),
            time: 0x0102_0304,
            bits: 0x1f07_ffff,
            cumulative_work: 0x1122_3344_5566_7788_99aa_bbcc_ddee_ff00,
        };
        let mut golden = [0u8; RECORD];
        golden[0] = 0xab;
        golden[31] = 0x01;
        golden[32..64].fill(0x5a);
        golden[64..68].copy_from_slice(&[0x04, 0x03, 0x02, 0x01]);
        golden[68..72].copy_from_slice(&[0xff, 0xff, 0x07, 0x1f]);
        golden[72..].copy_from_slice(&[
            0x00, 0xff, 0xee, 0xdd, 0xcc, 0xbb, 0xaa, 0x99, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33,
            0x22, 0x11,
        ]);
        assert_eq!(encode(&record), golden);
        assert_eq!(decode(&golden), record);
    }
}
