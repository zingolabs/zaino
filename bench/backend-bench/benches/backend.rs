//! The criterion entry point: every workload, each run against every backend.
//!
//! The backends are inputs, not a compile-time choice. Each workload opens one
//! `benchmark_group` and registers a benchmark per backend inside it, so a single
//! `cargo bench -p backend-bench` prints them next to each other and adding the
//! planned positional/mmap backend is one `register` line per workload.
//!
//! Run and baseline recipe: `bench/backend-bench/usage.md`.

use criterion::measurement::WallTime;
use criterion::{
    criterion_group, criterion_main, BenchmarkGroup, BenchmarkId, Criterion, Throughput,
};

use backend_bench::data::Shape;
use backend_bench::store::{height, snapshot, store_compose, store_compose_concurrent};
use backend_bench::target::{namespaces, BenchTarget, InMemory, Lmdb};
use backend_bench::workloads::{
    commit_batches, compose_block, compose_concurrent, read_by_hash, read_by_height,
    scan_namespace, Batches, Populated,
};
use backend_bench::Scale;

/// Logical volume for the run's scale, computed once so both backends report
/// throughput against the same denominator.
fn logical_bytes(scale: &Scale) -> u64 {
    Batches::generate(scale.shape, scale.seed, scale.blocks, scale.batch_size)
        .map(|batches| batches.logical_bytes())
        .unwrap_or_default()
}

/// A populated store for the read workloads, or a panic naming what failed:
/// a read benchmark over a store that did not build would measure misses.
fn populated<T: BenchTarget>(scale: &Scale) -> Populated<T> {
    let populated = Populated::<T>::build(scale.shape, scale.seed, scale.blocks, scale.batch_size)
        .unwrap_or_else(|e| panic!("populate {}: {e}", T::NAME));
    populated
        .verify()
        .unwrap_or_else(|e| panic!("verify {}: {e}", T::NAME));
    populated
}

/// The engine's write path: batches committed atomically, flushed once.
///
/// A fresh store per iteration, since a write benchmark against a store that
/// already holds the same keys measures overwrite, not growth. Generation and
/// store creation are in `iter_batched`'s setup, outside the timer.
fn commit<T: BenchTarget>(group: &mut BenchmarkGroup<'_, WallTime>, scale: &Scale) {
    group.bench_function(BenchmarkId::new(T::NAME, scale.label()), |b| {
        b.iter_batched(
            || {
                let batches =
                    Batches::generate(scale.shape, scale.seed, scale.blocks, scale.batch_size)
                        .unwrap_or_else(|e| panic!("generate: {e}"));
                let target = T::create().unwrap_or_else(|e| panic!("create {}: {e}", T::NAME));
                (target, batches)
            },
            |(target, batches)| {
                commit_batches(&target, batches).unwrap_or_else(|e| panic!("commit: {e}"))
            },
            criterion::BatchSize::PerIteration,
        );
    });
}

/// One point read by height key.
fn point_height<T: BenchTarget>(group: &mut BenchmarkGroup<'_, WallTime>, scale: &Scale) {
    let store = populated::<T>(scale);
    let reader = store.reader().unwrap_or_else(|e| panic!("reader: {e}"));
    let mut index = 0usize;
    group.bench_function(BenchmarkId::new(T::NAME, scale.label()), |b| {
        b.iter(|| {
            index = index.wrapping_add(1);
            read_by_height(&reader, store.height_key(index)).unwrap_or_else(|e| panic!("read: {e}"))
        });
    });
}

/// One point read by 32-byte hash key — a different key distribution from the
/// height-ordered namespaces.
fn point_hash<T: BenchTarget>(group: &mut BenchmarkGroup<'_, WallTime>, scale: &Scale) {
    let store = populated::<T>(scale);
    let reader = store.reader().unwrap_or_else(|e| panic!("reader: {e}"));
    let mut index = 0usize;
    group.bench_function(BenchmarkId::new(T::NAME, scale.label()), |b| {
        b.iter(|| {
            index = index.wrapping_add(1);
            read_by_hash(&reader, store.hash_key(index)).unwrap_or_else(|e| panic!("read: {e}"))
        });
    });
}

/// The store's compose-on-read fan-out for one served compact block.
fn compose<T: BenchTarget>(group: &mut BenchmarkGroup<'_, WallTime>, scale: &Scale) {
    let store = populated::<T>(scale);
    let reader = store.reader().unwrap_or_else(|e| panic!("reader: {e}"));
    let mut index = 0usize;
    group.bench_function(BenchmarkId::new(T::NAME, scale.label()), |b| {
        b.iter(|| {
            index = index.wrapping_add(1);
            compose_block(&reader, store.height_key(index))
                .unwrap_or_else(|e| panic!("compose: {e}"))
        });
    });
}

/// Threads for the concurrent workloads (`BACKEND_BENCH_THREADS`, default 16),
/// and how many blocks each composes per measured run.
fn concurrency() -> (usize, usize) {
    let threads = std::env::var("BACKEND_BENCH_THREADS")
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(16);
    (threads, 500)
}

/// The backend fan-out from many threads at once, a reader per block: shows
/// contention a single-threaded loop cannot.
fn compose_many<T: BenchTarget + Sync>(group: &mut BenchmarkGroup<'_, WallTime>, scale: &Scale) {
    let store = populated::<T>(scale);
    let (threads, per_thread) = concurrency();
    group.bench_function(BenchmarkId::new(T::NAME, scale.label()), |b| {
        b.iter_custom(|iters| {
            (0..iters)
                .map(|_| {
                    compose_concurrent(&store, threads, per_thread)
                        .unwrap_or_else(|e| panic!("compose: {e}"))
                })
                .sum()
        });
    });
}

/// One served compact block through the real store: freshness checks, decodes
/// and assembly on top of the backend fan-out.
fn store_one<T: BenchTarget>(group: &mut BenchmarkGroup<'_, WallTime>, scale: &Scale)
where
    T::Backend: Clone + 'static,
{
    let store = populated::<T>(scale);
    let snapshot = snapshot(&store).unwrap_or_else(|e| panic!("snapshot: {e}"));
    let mut index = 0usize;
    group.bench_function(BenchmarkId::new(T::NAME, scale.label()), |b| {
        b.iter(|| {
            index = index.wrapping_add(1);
            store_compose(&snapshot, height(&store, index))
                .unwrap_or_else(|e| panic!("store read: {e}"))
        });
    });
}

/// The store read from many threads at once, a snapshot per thread.
fn store_many<T: BenchTarget + Sync>(group: &mut BenchmarkGroup<'_, WallTime>, scale: &Scale)
where
    T::Backend: Clone + 'static,
{
    let store = populated::<T>(scale);
    let (threads, per_thread) = concurrency();
    group.bench_function(BenchmarkId::new(T::NAME, scale.label()), |b| {
        b.iter_custom(|iters| {
            (0..iters)
                .map(|_| {
                    store_compose_concurrent(&store, threads, per_thread)
                        .unwrap_or_else(|e| panic!("store read: {e}"))
                })
                .sum()
        });
    });
}

/// A whole-namespace walk — where a ranged read would slot in.
fn scan<T: BenchTarget>(group: &mut BenchmarkGroup<'_, WallTime>, scale: &Scale) {
    let store = populated::<T>(scale);
    let reader = store.reader().unwrap_or_else(|e| panic!("reader: {e}"));
    let headers = namespaces()
        .into_iter()
        .find(|namespace| namespace.as_str() == "headers")
        .unwrap_or_else(|| panic!("the headers namespace is declared"));
    group.bench_function(BenchmarkId::new(T::NAME, scale.label()), |b| {
        b.iter(|| scan_namespace(&reader, headers).unwrap_or_else(|e| panic!("scan: {e}")));
    });
}

/// Print what each backend occupies on disk beside the logical bytes written, so
/// write amplification is visible. Not a timed benchmark: it is one measurement
/// per backend, taken once per run and printed before the groups.
fn growth(_c: &mut Criterion) {
    let scale = Scale::from_env();
    println!("\n-- on-disk growth [{}]", scale.label());
    report_growth::<Lmdb>(&scale);
    report_growth::<InMemory>(&scale);
    println!();
}

fn report_growth<T: BenchTarget>(scale: &Scale) {
    let store = populated::<T>(scale);
    let (logical, on_disk) = (store.logical_bytes(), store.on_disk_bytes());
    // Integer percent: a float here would only add spurious precision.
    let amplification = on_disk
        .checked_mul(100)
        .map(|scaled| scaled / logical.max(1))
        .unwrap_or_default();
    println!(
        "   {:<10} blocks={} logical={} KiB on-disk={} KiB ({}% of logical)",
        T::NAME,
        store.blocks(),
        logical / 1024,
        on_disk / 1024,
        amplification,
    );
    tracing_free_event(T::NAME, scale.shape, store.blocks(), logical, on_disk);
}

/// The growth figures as one machine-readable line, for a run whose output is
/// collected rather than read.
fn tracing_free_event(backend: &str, shape: Shape, blocks: usize, logical: u64, on_disk: u64) {
    println!(
        "   growth_json {{\"backend\":\"{backend}\",\"shape\":\"{}\",\"blocks\":{blocks},\
         \"logical_bytes\":{logical},\"on_disk_bytes\":{on_disk}}}",
        shape.as_str()
    );
}

fn commit_batches_group(c: &mut Criterion) {
    let scale = Scale::from_env();
    let mut group = c.benchmark_group("commit_batches");
    group.throughput(Throughput::Bytes(logical_bytes(&scale)));
    commit::<Lmdb>(&mut group, &scale);
    commit::<InMemory>(&mut group, &scale);
    group.finish();
}

fn read_by_height_group(c: &mut Criterion) {
    let scale = Scale::from_env();
    let mut group = c.benchmark_group("read_by_height");
    group.throughput(Throughput::Elements(1));
    point_height::<Lmdb>(&mut group, &scale);
    point_height::<InMemory>(&mut group, &scale);
    group.finish();
}

fn read_by_hash_group(c: &mut Criterion) {
    let scale = Scale::from_env();
    let mut group = c.benchmark_group("read_by_hash");
    group.throughput(Throughput::Elements(1));
    point_hash::<Lmdb>(&mut group, &scale);
    point_hash::<InMemory>(&mut group, &scale);
    group.finish();
}

fn compose_group(c: &mut Criterion) {
    let scale = Scale::from_env();
    let mut group = c.benchmark_group("compose_block");
    // One served compact block per iteration, whatever the fan-out costs.
    group.throughput(Throughput::Elements(1));
    compose::<Lmdb>(&mut group, &scale);
    compose::<InMemory>(&mut group, &scale);
    group.finish();
}

fn compose_concurrent_group(c: &mut Criterion) {
    let scale = Scale::from_env();
    let (threads, per_thread) = concurrency();
    let mut group = c.benchmark_group(format!("compose_block_x{threads}"));
    group.throughput(Throughput::Elements(
        u64::try_from(threads * per_thread).unwrap_or(u64::MAX),
    ));
    compose_many::<Lmdb>(&mut group, &scale);
    compose_many::<InMemory>(&mut group, &scale);
    group.finish();
}

fn store_compose_group(c: &mut Criterion) {
    let scale = Scale::from_env();
    let mut group = c.benchmark_group("store_compose");
    group.throughput(Throughput::Elements(1));
    store_one::<Lmdb>(&mut group, &scale);
    store_one::<InMemory>(&mut group, &scale);
    group.finish();
}

fn store_compose_concurrent_group(c: &mut Criterion) {
    let scale = Scale::from_env();
    let (threads, per_thread) = concurrency();
    let mut group = c.benchmark_group(format!("store_compose_x{threads}"));
    group.throughput(Throughput::Elements(
        u64::try_from(threads * per_thread).unwrap_or(u64::MAX),
    ));
    store_many::<Lmdb>(&mut group, &scale);
    store_many::<InMemory>(&mut group, &scale);
    group.finish();
}

fn scan_group(c: &mut Criterion) {
    let scale = Scale::from_env();
    let mut group = c.benchmark_group("scan_namespace");
    group.throughput(Throughput::Elements(u64::from(scale.blocks)));
    scan::<Lmdb>(&mut group, &scale);
    scan::<InMemory>(&mut group, &scale);
    group.finish();
}

// `criterion_group!` expands to a public function the harness calls; the crate
// warns on undocumented public items, and a macro invocation takes no doc
// comment, so the exemption is stated here rather than crate-wide.
#[allow(missing_docs)]
mod harness_entry {
    use super::*;

    criterion_group!(
        backend,
        growth,
        commit_batches_group,
        read_by_height_group,
        read_by_hash_group,
        compose_group,
        compose_concurrent_group,
        store_compose_group,
        store_compose_concurrent_group,
        scan_group,
    );
}

criterion_main!(harness_entry::backend);
