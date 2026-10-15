//! `DiskView::footprint` on a real filesystem (page cache = the kernel's, read by `mincore`)

#![cfg(all(target_os = "linux", feature = "testing"))]

use std::{fs, num::NonZeroUsize, path::Path};

use zaino_persistence::{
    fs::{RealFs, SimFs},
    DiskEngine, IndexKind, LsmConfig, MapTable, PersistenceEngine, Schema, SequenceTable, Store,
    TableFootprint, Tables, Width,
};
use zaino_primitives::types::BlockRef;
use zcash_protocol::consensus::{NetworkType, H0};
use zcash_protocol::BlockHash;

const FIXED: SequenceTable = SequenceTable::new(0, "fixed", Width::fixed(32)).cache_writes();
const VARIABLE: SequenceTable = SequenceTable::new(1, "variable", Width::Variable);
const DROPPED: MapTable = MapTable::point(0, "dropped", 36, 8);
const CACHED: MapTable = MapTable::point(1, "cached", 36, 8).cache_writes();
const SCHEMA: Schema = Schema::new(
    IndexKind::UtxoSet,
    1,
    NetworkType::Regtest,
    Tables::new(&[FIXED, VARIABLE], &[DROPPED, CACHED]),
);

/// - bytes = durable data (records + 8-byte ends, every listed segment), never the files'
///   preallocated tails
/// - cached right after the commit: each `cache_writes` table all of it; a default sequence none
///   (dropped once synced), a default map only what opening the view reads (summary + filters)
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
        changes.map(DROPPED).insert(&key, &n.to_le_bytes());
        changes.map(CACHED).insert(&key, &n.to_le_bytes());
        store.apply(changes);
    }
    let mut height = H0;
    for n in 0..20_000u64 {
        let at = BlockRef { height, hash: BlockHash([0; 32]) };
        fill(&mut store, at, n);
        fill(&mut sim, at, n);
        height = height + 1;
    }
    store.commit().expect("commit");
    sim.commit().expect("commit sim");

    let segments = |map: &str| -> u64 {
        fs::read_dir(dir.path().join(map))
            .expect("map dir")
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "seg"))
            .map(|entry| fs::metadata(entry.path()).expect("segment").len())
            .sum()
    };
    let (dropped, cached) = (segments("dropped"), segments("cached"));
    assert!(dropped > 0 && dropped == cached, "same rows, same segment bytes");
    let variable: u64 = (0..20_000u64).map(|n| 100 + n % 200).sum::<u64>() + 20_000 * 8;
    let footprint = store.durable().footprint();
    let opened = footprint.iter().find(|table| table.table == "dropped").and_then(|t| t.cached);
    assert!(opened.is_some_and(|bytes| bytes < dropped / 10), "dropped: {footprint:?}");
    let tables = [
        ("fixed", 20_000 * 32, None),
        ("variable", variable, Some(0)),
        ("dropped", dropped, opened),
        ("cached", cached, None),
    ];
    let expected: Vec<TableFootprint> = tables
        .iter()
        .map(|&(table, bytes, cached)| TableFootprint {
            table,
            bytes,
            cached: cached.or(Some(bytes)),
        })
        .collect();
    assert_eq!(footprint, expected);

    let simulated = sim.durable().footprint();
    let unknowable: Vec<TableFootprint> =
        expected.iter().map(|table| TableFootprint { cached: None, ..*table }).collect();
    assert_eq!(simulated, unknowable);
}
