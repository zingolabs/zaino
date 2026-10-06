//! The real sync engine drives the LMDB backend without an out-of-order append.
//!
//! The engine extracts a batch's blocks in parallel (rayon), so a batch's puts
//! for one walk-ordered namespace reach `commit` in completion order, not key
//! order. `MDB_APPEND` demands each key beat the current last one, so a batch
//! of more than one block used to fail its first commit with
//! `OutOfOrderAppend` — on a *different* namespace each run, since the race
//! decides which one lands out of order. The in-memory backend never showed it
//! (it does not append), and the hand-written LMDB fixtures all committed
//! pre-sorted keys.
//!
//! This drives the engine over an LMDB temp-dir backend for several
//! multi-block batches and asserts it succeeds and lands exactly what the
//! in-memory backend does.

use std::path::Path;

use zaino_backend_lmdb::{LmdbBackend, LmdbConfig};
use zaino_persistence::in_memory::InMemoryBackend;
use zaino_persistence::{Backend, BackendReader, Namespace, NamespaceSpec, RawKey, RawValue};
use zaino_persistence_codec::reserved_namespaces;
use zaino_sync::engine::{EngineConfig, SyncEngine};
use zaino_sync::primitives::BlockHeight;
use zaino_sync::provisioner::Provisioner;
use zaino_sync::testing::{toy_pipelines, MockProvisioner, TestBlockContext};

/// The toy set's namespaces (value is walk-ordered, count and running_sum are
/// scattered) plus the reserved meta namespaces the engine stamps.
fn specs() -> Vec<NamespaceSpec> {
    toy_pipelines()
        .namespace_specs()
        .into_iter()
        .chain(reserved_namespaces().map(NamespaceSpec::meta))
        .collect()
}

fn open_lmdb(path: &Path) -> LmdbBackend {
    LmdbBackend::open(LmdbConfig {
        path: path.to_path_buf(),
        map_size_bytes: 16 << 20,
        namespaces: specs(),
    })
    .expect("open lmdb")
}

/// Sync blocks `[0, count)` through a fresh engine over `backend`, in batches of
/// `batch` blocks, committing each atomically.
fn sync<B: Backend + Clone>(backend: &B, count: u64, batch: u32) {
    let blocks: Vec<TestBlockContext> = MockProvisioner::identity()
        .provision_range(BlockHeight::new(0), BlockHeight::new(count - 1))
        .expect("provision");
    let mut engine = SyncEngine::from_pipelines(
        toy_pipelines(),
        backend.clone(),
        EngineConfig {
            batch_size: batch,
            start_height: BlockHeight::new(0),
        },
    )
    .expect("engine builds");
    engine.sync_range(blocks).expect("sync succeeds");
}

/// Every declared namespace, scanned to owned sorted pairs.
fn dump<B: Backend>(backend: &B) -> Vec<(Namespace, Vec<(RawKey, RawValue)>)> {
    let reader = backend.reader().expect("reader");
    specs()
        .iter()
        .map(|spec| (spec.namespace, reader.scan(spec.namespace).expect("scan")))
        .collect()
}

/// Multi-block batches over LMDB succeed (no `OutOfOrderAppend` despite parallel
/// extraction) and store exactly what the in-memory backend does.
#[test]
fn engine_drives_lmdb_over_multi_block_batches() {
    // A batch bigger than one block is the trigger: each batch's walk-ordered
    // puts span several heights, extracted in parallel. 13 blocks over batches
    // of 4 gives several multi-block commits plus a short tail.
    const BLOCKS: u64 = 13;
    const BATCH: u32 = 4;

    let tmp = tempfile::tempdir().expect("tempdir");
    let lmdb = open_lmdb(tmp.path());
    sync(&lmdb, BLOCKS, BATCH);

    let memory = InMemoryBackend::new();
    sync(&memory, BLOCKS, BATCH);

    assert_eq!(
        dump(&lmdb),
        dump(&memory),
        "the LMDB backend stores exactly what the in-memory backend does"
    );

    // The walk-ordered namespace holds one ascending entry per height.
    let value_ns = Namespace::new("value");
    let value = lmdb.reader().expect("reader").scan(value_ns).expect("scan");
    assert_eq!(value.len(), usize::try_from(BLOCKS).expect("fits"));
    let heights: Vec<RawKey> = value.iter().map(|(key, _)| key.clone()).collect();
    let mut sorted = heights.clone();
    sorted.sort();
    assert_eq!(
        heights, sorted,
        "scan of a walk-ordered namespace is ascending"
    );
}
