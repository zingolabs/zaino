//! What one ChainHead publication allocates.
//!
//! The same scenarios as `publication.rs`, measured in allocations rather than
//! time. A run allocates the same number of times whatever else the machine is
//! doing, so this is the figure to compare between two commits: a difference
//! here is a difference in the code, not in the machine.
//!
//! ```text
//! cargo bench -p zaino-chain-head-service --features testing --bench allocations
//! ```
//!
//! Counting is not free, and it taxes the allocation-heavy side hardest, so
//! **times measured here mean nothing**; `publication.rs` measures those,
//! without the counting allocator.

mod harness;

use std::{alloc::System, sync::Arc};

use criterion::{
    criterion_group, criterion_main,
    measurement::{Measurement, ValueFormatter},
    BatchSize, Criterion, Throughput,
};
use harness::{anchored, synced, MockValidator, TICKS, WINDOW};
use stats_alloc::{StatsAlloc, INSTRUMENTED_SYSTEM};
use tokio::runtime::Runtime;
use zaino_chain_head::ChainHeadBlockService as _;
use zaino_chain_head_service::MapBackedSnapshot;

/// The system allocator, counting what passes through it.
#[global_allocator]
static ALLOCATOR: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

/// Counts allocations rather than time.
struct Allocations;

impl Measurement for Allocations {
    type Intermediate = usize;
    type Value = usize;

    fn start(&self) -> Self::Intermediate {
        ALLOCATOR.stats().allocations
    }

    fn end(&self, start: Self::Intermediate) -> Self::Value {
        ALLOCATOR.stats().allocations.saturating_sub(start)
    }

    fn add(&self, first: &Self::Value, second: &Self::Value) -> Self::Value {
        first.saturating_add(*second)
    }

    fn zero(&self) -> Self::Value {
        0
    }

    fn to_f64(&self, value: &Self::Value) -> f64 {
        u32::try_from(*value).map_or(f64::INFINITY, f64::from)
    }

    fn formatter(&self) -> &dyn ValueFormatter {
        &AllocationFormatter
    }
}

struct AllocationFormatter;

impl ValueFormatter for AllocationFormatter {
    fn scale_values(&self, _typical: f64, _values: &mut [f64]) -> &'static str {
        "allocations"
    }

    fn scale_throughputs(
        &self,
        _typical: f64,
        _throughput: &Throughput,
        _values: &mut [f64],
    ) -> &'static str {
        "allocations"
    }

    fn scale_for_machines(&self, _values: &mut [f64]) -> &'static str {
        "allocations"
    }
}

/// Counts the bytes a scenario holds when it ends: what it allocated, less
/// what it freed.
///
/// This is memory still in use, not memory touched — the figure that decides
/// how much a chain head costs a running indexer. It ignores allocator
/// overhead and fragmentation, so it is a floor rather than resident set size.
/// A scenario that frees more than it allocates reads as zero.
struct RetainedBytes;

/// The allocator's running totals, as bytes in and bytes out.
fn bytes_in_out() -> (usize, usize) {
    let stats = ALLOCATOR.stats();
    (stats.bytes_allocated, stats.bytes_deallocated)
}

impl Measurement for RetainedBytes {
    type Intermediate = (usize, usize);
    type Value = usize;

    fn start(&self) -> Self::Intermediate {
        bytes_in_out()
    }

    fn end(&self, start: Self::Intermediate) -> Self::Value {
        let (allocated, freed) = bytes_in_out();
        allocated
            .saturating_sub(start.0)
            .saturating_sub(freed.saturating_sub(start.1))
    }

    fn add(&self, first: &Self::Value, second: &Self::Value) -> Self::Value {
        first.saturating_add(*second)
    }

    fn zero(&self) -> Self::Value {
        0
    }

    fn to_f64(&self, value: &Self::Value) -> f64 {
        u32::try_from(*value).map_or(f64::INFINITY, f64::from)
    }

    fn formatter(&self) -> &dyn ValueFormatter {
        &ByteFormatter
    }
}

struct ByteFormatter;

/// Bytes in a mebibyte, and in a kibibyte.
const MIB: f64 = 1024.0 * 1024.0;
const KIB: f64 = 1024.0;

impl ByteFormatter {
    fn scale(typical: f64, values: &mut [f64]) -> &'static str {
        let (divisor, unit) = if typical >= MIB {
            (MIB, "MiB held")
        } else if typical >= KIB {
            (KIB, "KiB held")
        } else {
            (1.0, "B held")
        };
        for value in values {
            *value /= divisor;
        }
        unit
    }
}

impl ValueFormatter for ByteFormatter {
    fn scale_values(&self, typical: f64, values: &mut [f64]) -> &'static str {
        Self::scale(typical, values)
    }

    fn scale_throughputs(
        &self,
        typical: f64,
        _throughput: &Throughput,
        values: &mut [f64],
    ) -> &'static str {
        Self::scale(typical, values)
    }

    fn scale_for_machines(&self, _values: &mut [f64]) -> &'static str {
        "bytes held"
    }
}

fn publication(criterion: &mut Criterion<Allocations>) {
    let runtime = Runtime::new().expect("a tokio runtime");
    let mut group = criterion.benchmark_group("chain-head-allocations");
    group.sample_size(10);

    group.bench_function("initial sync", |bencher| {
        bencher.iter_batched(
            || {
                let validator = MockValidator::linear(WINDOW);
                let service = anchored(&runtime, &validator);
                (validator, service)
            },
            |(_validator, service)| {
                runtime.block_on(async { service.advance_once().await.expect("advance succeeds") });
            },
            BatchSize::LargeInput,
        );
    });

    group.bench_function("8 publications held", |bencher| {
        bencher.iter_batched(
            || {
                let validator = MockValidator::linear(WINDOW);
                let service = synced(&runtime, &validator);
                (validator, service)
            },
            |(validator, service)| {
                let mut held: Vec<Arc<MapBackedSnapshot>> =
                    Vec::with_capacity(usize::try_from(TICKS).expect("the tick count fits usize"));
                for _ in 0..TICKS {
                    validator.extend();
                    runtime.block_on(async {
                        service.advance_once().await.expect("advance succeeds")
                    });
                    held.push(service.subscriber().current());
                }
                held
            },
            BatchSize::LargeInput,
        );
    });

    group.finish();
}

/// The same scenarios, counted in bytes still held when each ends.
fn memory(criterion: &mut Criterion<RetainedBytes>) {
    let runtime = Runtime::new().expect("a tokio runtime");
    let mut group = criterion.benchmark_group("chain-head-memory");
    group.sample_size(10);

    // What a synced chain head holds: one window of blocks.
    group.bench_function("a synced window", |bencher| {
        bencher.iter_batched(
            || MockValidator::linear(WINDOW),
            |validator| synced(&runtime, &validator),
            BatchSize::LargeInput,
        );
    });

    // What it holds when a reader keeps every published snapshot.
    group.bench_function("8 publications held", |bencher| {
        bencher.iter_batched(
            || {
                let validator = MockValidator::linear(WINDOW);
                let service = synced(&runtime, &validator);
                (validator, service)
            },
            |(validator, service)| {
                let mut held: Vec<Arc<MapBackedSnapshot>> =
                    Vec::with_capacity(usize::try_from(TICKS).expect("the tick count fits usize"));
                for _ in 0..TICKS {
                    validator.extend();
                    runtime.block_on(async {
                        service.advance_once().await.expect("advance succeeds")
                    });
                    held.push(service.subscriber().current());
                }
                (service, held)
            },
            BatchSize::LargeInput,
        );
    });

    group.finish();
}

criterion_group!(
    name = allocations;
    config = Criterion::default().with_measurement(Allocations);
    targets = publication
);
criterion_group!(
    name = retained;
    config = Criterion::default().with_measurement(RetainedBytes);
    targets = memory
);
criterion_main!(allocations, retained);
