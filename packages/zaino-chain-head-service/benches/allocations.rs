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

criterion_group!(
    name = benches;
    config = Criterion::default().with_measurement(Allocations);
    targets = publication
);
criterion_main!(benches);
