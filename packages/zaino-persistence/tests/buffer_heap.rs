//! `DiskStore::buffered_bytes` vs the heap a `DiskStore` buffer really holds
//!
//! - own binary: `#[global_allocator]` counts every allocation of this process only

#![cfg(target_os = "linux")]

use std::{
    alloc::{GlobalAlloc, Layout, System},
    num::NonZeroUsize,
    sync::atomic::{AtomicUsize, Ordering},
};

use zaino_persistence::{
    fs::RealFs, BlockChanges, DiskEngine, MapTable, PersistenceEngine, Schema, SequenceTable,
    Store, Tables, Width,
};
use zaino_primitives::types::BlockRef;
use zcash_protocol::consensus::H0;
use zcash_protocol::BlockHash;

/// Live heap: usable size + glibc's 8 B chunk header per allocation
static LIVE: AtomicUsize = AtomicUsize::new(0);

struct Counting;

fn chunk(ptr: *mut u8) -> usize {
    // SAFETY: `ptr` = live allocation of `System` (malloc on Linux)
    unsafe { libc::malloc_usable_size(ptr.cast()) + 8 }
}

// SAFETY: forwards to `System`, only counts
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: caller's `layout` contract, passed through
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            LIVE.fetch_add(chunk(ptr), Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(chunk(ptr), Ordering::Relaxed);
        // SAFETY: caller's contract, passed through
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

const FIXED: SequenceTable = SequenceTable::new(0, "fixed", Width::fixed(32));
const VARIABLE: SequenceTable = SequenceTable::new(1, "variable", Width::Variable);
const MAP: MapTable = MapTable::point(0, "map", 36, 8);
const SCHEMA: Schema =
    Schema::new("outputs", 1, "regtest", Tables::new(&[FIXED, VARIABLE], &[MAP]));

/// Uniform, distinct per `n` (random insert order into the map)
fn key(n: u64) -> [u8; 36] {
    let mut key = [0; 36];
    key[..8].copy_from_slice(&n.wrapping_mul(0x9e37_79b9_7f4a_7c15).to_be_bytes());
    key[28..].copy_from_slice(&n.to_be_bytes());
    key
}

/// Single test: nothing else allocates while one runs
#[test]
fn buffered_bytes_track_the_real_heap_of_each_table_shape_within_30_percent() {
    type Fill = fn(&mut BlockChanges, u64);
    let cases: [(&str, u64, Fill); 6] = [
        ("fixed 32 B, 1/block", 1, |out, n| out.sequence(FIXED).append(&key(n)[..32])),
        ("fixed 32 B, 100/block", 100, |out, n| out.sequence(FIXED).append(&key(n)[..32])),
        ("variable 100..300 B, 1/block", 1, |out, n| {
            out.sequence(VARIABLE)
                .append(&vec![n.to_le_bytes()[0]; 100 + usize::try_from(n % 200).expect("< 200")])
        }),
        ("variable 100..300 B, 100/block", 100, |out, n| {
            out.sequence(VARIABLE)
                .append(&vec![n.to_le_bytes()[0]; 100 + usize::try_from(n % 200).expect("< 200")])
        }),
        ("map 36 → 8 B, 1/block", 1, |out, n| out.map(MAP).insert(&key(n), &n.to_le_bytes())),
        ("map 36 → 8 B, 100/block", 100, |out, n| out.map(MAP).insert(&key(n), &n.to_le_bytes())),
    ];
    const ROWS: u64 = 100_000;

    let mut report = Vec::new();
    for (shape, per_block, fill) in cases {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = DiskEngine::new(RealFs::shared(), zaino_persistence::LsmConfig::default())
            .open(dir.path(), &SCHEMA, NonZeroUsize::MAX)
            .expect("open");
        let before = LIVE.load(Ordering::Relaxed);
        let mut height = H0;
        for first in (0..ROWS).step_by(usize::try_from(per_block).expect("<= 100")) {
            let mut changes = store.changes(BlockRef { height, hash: BlockHash([0; 32]) });
            for n in first..first + per_block {
                fill(&mut changes, n);
            }
            store.apply(changes);
            height = height + 1;
        }
        let heap = LIVE.load(Ordering::Relaxed) - before;
        let accounted = store.buffered_bytes();
        let rows = usize::try_from(ROWS).expect("100 000 fits usize");
        let percent = (accounted * 100).checked_div(heap);
        report.push((shape, heap / rows, accounted / rows, percent));
    }
    let off =
        |percent: &Option<usize>| !percent.is_some_and(|percent| (70..=130).contains(&percent));
    let off: Vec<_> = report.iter().filter(|(.., percent)| off(percent)).collect();
    assert!(
        off.is_empty(),
        "(shape, heap/row, accounted/row, accounted % of heap) off by > 30%: {report:#?}"
    );
}
