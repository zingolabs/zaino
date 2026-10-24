//! `write_buffer` vs the heap the buffer really holds when a commit writes it
//!
//! - own binary: `#[global_allocator]` counts every allocation of this process only

// glibc's malloc: `malloc_usable_size` + its 8-byte chunk header is what an allocation costs
#![cfg(all(target_os = "linux", target_env = "gnu"))]
// the counting allocator: forwards to `System`, adds `malloc_usable_size`
#![allow(unsafe_code)]

use std::{
    alloc::{GlobalAlloc, Layout, System},
    num::{NonZeroU32, NonZeroUsize},
    sync::atomic::{AtomicUsize, Ordering},
};

use zaino_persistence::{
    sequence_key, Backend, BackendWriter, Namespace, NamespaceSpec, Width, WriteOp,
};
use zaino_persistence_lsm_tree::{LsmConfig, LsmTreeBackend, LsmTreeConfig};
use zaino_primitives::types::{BlockHash, BlockRef, Height};

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

const FIXED: Namespace = Namespace::new("fixed");
const VARIABLE: Namespace = Namespace::new("variable");
const MAP: Namespace = Namespace::new("map");

/// Uniform, distinct per `n` (random insert order into the map)
fn key(n: u64) -> Vec<u8> {
    let mut key = vec![0; 36];
    key[..8].copy_from_slice(&n.wrapping_mul(0x9e37_79b9_7f4a_7c15).to_be_bytes());
    key[28..].copy_from_slice(&n.to_be_bytes());
    key
}

fn put(namespace: Namespace, key: Vec<u8>, value: Vec<u8>) -> WriteOp {
    WriteOp::Put {
        namespace,
        key,
        value,
    }
}

const WRITE_BUFFER: NonZeroUsize = NonZeroUsize::new(4 << 20).expect("non-zero");

/// Single test: nothing else allocates while one runs
#[test]
fn a_commit_writes_the_buffer_when_its_real_heap_reaches_write_buffer_within_10_percent() {
    type Fill = fn(u64) -> WriteOp;
    let cases: [(&str, u64, Fill); 6] = [
        ("fixed 32 B, 1/commit", 1, |n| {
            put(FIXED, sequence_key(n), key(n)[..32].to_vec())
        }),
        ("fixed 32 B, 100/commit", 100, |n| {
            put(FIXED, sequence_key(n), key(n)[..32].to_vec())
        }),
        ("variable 100..300 B, 1/commit", 1, |n| {
            put(
                VARIABLE,
                sequence_key(n),
                vec![n as u8; 100 + (n % 200) as usize],
            )
        }),
        ("variable 100..300 B, 100/commit", 100, |n| {
            put(
                VARIABLE,
                sequence_key(n),
                vec![n as u8; 100 + (n % 200) as usize],
            )
        }),
        ("map 36 → 8 B, 1/commit", 1, |n| {
            put(MAP, key(n), n.to_le_bytes().to_vec())
        }),
        ("map 36 → 8 B, 100/commit", 100, |n| {
            put(MAP, key(n), n.to_le_bytes().to_vec())
        }),
    ];
    const ROWS: u64 = 100_000;
    let width = |n| NonZeroU32::new(n).expect("non-zero");
    let specs = [
        NamespaceSpec::sequence(FIXED, Width::Fixed(width(32))),
        NamespaceSpec::sequence(VARIABLE, Width::Variable),
        NamespaceSpec::map(MAP, width(36), width(8), false),
    ];

    let mut report = Vec::new();
    for (shape, per_commit, fill) in cases {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = LsmTreeConfig {
            name: "heap",
            format: 1,
            namespaces: &specs,
            tuning: &[],
            lsm: LsmConfig::default(),
            write_buffer: WRITE_BUFFER,
            threads: NonZeroUsize::new(2),
        };
        let backend = LsmTreeBackend::open_real(dir.path(), &config).expect("open");
        let mut writer = backend.writer().expect("writer");
        let before = LIVE.load(Ordering::Relaxed) as i64;
        let mut buffered = 0;
        let mut written = false;
        for (at, first) in (0..ROWS).step_by(per_commit as usize).enumerate() {
            // signed: memory allocated at open can be freed as the buffer reshapes
            buffered = LIVE.load(Ordering::Relaxed) as i64 - before;
            let ops = (first..first + per_commit).map(fill).collect();
            let tip = BlockRef {
                height: Height::try_from(at as u32).expect("height"),
                hash: BlockHash::from([0; 32]),
            };
            writer.commit(tip, ops).expect("commit");
            let footprint = backend.footprint().expect("footprint");
            if footprint.iter().any(|namespace| namespace.bytes > 0) {
                written = true;
                break;
            }
        }
        assert!(written, "{shape}: the buffer reached write_buffer");
        let ratio = buffered as f64 / WRITE_BUFFER.get() as f64;
        report.push((shape, buffered, ratio));
    }
    let off: Vec<_> = report
        .iter()
        .filter(|(.., ratio)| !(0.9..=1.1).contains(ratio))
        .collect();
    assert!(
        off.is_empty(),
        "(shape, real heap when written, ratio to write_buffer) off by > 10%: {report:#?}"
    );
}
