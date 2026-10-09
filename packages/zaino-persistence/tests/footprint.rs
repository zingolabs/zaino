//! `DiskView::footprint` on a real filesystem (page cache = the kernel's, read by `mincore`)

#![cfg(all(target_os = "linux", feature = "testing"))]

use std::{fs, num::NonZeroUsize, path::Path};

use zaino_persistence::{
    fs::{RealFs, SimFs},
    DiskEngine, IndexKind, LsmConfig, MapTable, PersistenceEngine, Schema, SequenceTable, Store,
    TableFootprint, Tables, Width,
};
use zaino_primitives::types::{BlockHash, BlockRef, Height};
use zcash_protocol::consensus::NetworkType;

const FIXED: SequenceTable = SequenceTable::new(0, "fixed", Width::fixed(32));
const VARIABLE: SequenceTable = SequenceTable::new(1, "variable", Width::Variable);
const MAP: MapTable = MapTable::new(0, "map", Width::fixed(36), Width::fixed(8), 0);
const SCHEMA: Schema = Schema::new(
    IndexKind::ValueBalance,
    1,
    NetworkType::Regtest,
    Tables::new(&[FIXED, VARIABLE], &[MAP]),
);

/// - bytes = committed data (records + 8-byte ends, every listed segment), never the files'
///   preallocated tails
/// - cached = all of it right after a commit wrote it (written pages stay in page cache), never
///   above bytes
/// - `SimFs`: same bytes, cached unknowable (`None`)
#[test]
fn footprint_reports_each_tables_committed_bytes_and_their_page_cache_share() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut store = DiskEngine::new(RealFs::shared(), LsmConfig::default())
        .open(dir.path(), &SCHEMA, NonZeroUsize::MAX)
        .expect("open");
    let mut sim = DiskEngine::new(SimFs::new(), LsmConfig::default())
        .open(Path::new("/sim"), &SCHEMA, NonZeroUsize::MAX)
        .expect("open sim");
    fn fill<S: Store>(store: &mut S, at: BlockRef, n: u64) {
        let mut changes = store.changes(at);
        changes.sequence(FIXED).append(&[n as u8; 32]);
        changes.sequence(VARIABLE).append(&vec![n as u8; 100 + (n % 200) as usize]);
        let mut key = [0u8; 36];
        key[..8].copy_from_slice(&n.wrapping_mul(0x9e37_79b9_7f4a_7c15).to_be_bytes());
        changes.map(MAP).insert(&key, &n.to_le_bytes());
        store.apply(changes);
    }
    let mut height = Height::GENESIS;
    for n in 0..20_000u64 {
        let at = BlockRef { height, hash: BlockHash::from([0; 32]) };
        fill(&mut store, at, n);
        fill(&mut sim, at, n);
        height = height.next();
    }
    store.commit().expect("commit");
    sim.commit().expect("commit sim");

    let segments: u64 = fs::read_dir(dir.path().join("map"))
        .expect("map dir")
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "seg"))
        .map(|entry| fs::metadata(entry.path()).expect("segment").len())
        .sum();
    let variable: u64 = (0..20_000u64).map(|n| 100 + n % 200).sum::<u64>() + 20_000 * 8;
    let committed = [("fixed", 20_000 * 32), ("variable", variable), ("map", segments)];
    let expected: Vec<TableFootprint> = committed
        .iter()
        .map(|&(table, bytes)| TableFootprint { table, bytes, cached: Some(bytes) })
        .collect();
    assert_eq!(store.committed().footprint(), expected);
    assert!(segments > 0, "the map wrote a segment");

    let simulated = sim.committed().footprint();
    let unknowable: Vec<TableFootprint> =
        expected.iter().map(|table| TableFootprint { cached: None, ..*table }).collect();
    assert_eq!(simulated, unknowable);
}
