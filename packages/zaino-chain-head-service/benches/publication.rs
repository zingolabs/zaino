//! What one ChainHead publication costs.
//!
//! The chain head's per-tick work is dominated by the snapshot it builds: it
//! copies the published one, extends it, trims it, and publishes the result,
//! while readers keep earlier ones alive. This measures that path through the
//! service, so a change to how the snapshot is stored shows up here.
//!
//! The validator is a mock over the `zaino-source` ports, answering from
//! memory, so no I/O is measured. Blocks carry [`TRANSACTIONS`] transactions
//! each, because a snapshot copy copies what the blocks own.
//!
//! ```text
//! cargo bench -p zaino-chain-head-service --features testing
//! ```
//!
//! Criterion stores the results, so a later run reports the change against the
//! stored ones. To compare two commits: run it on the first with
//! `-- --save-baseline <name>`, then on the second with `-- --baseline <name>`.

mod harness;

use std::sync::Arc;

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use harness::{anchored, synced, MockValidator, TICKS, WINDOW};
use tokio::runtime::Runtime;
use zaino_chain_head::ChainHeadBlockService as _;
use zaino_chain_head_service::MapBackedSnapshot;

fn publication(criterion: &mut Criterion) {
    let runtime = Runtime::new().expect("a tokio runtime");
    let mut group = criterion.benchmark_group("chain-head");
    group.sample_size(20);

    // Filling the window from the anchor: what a boot or a long catch-up does.
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

    // Steady state: one new block per tick, with every published snapshot held,
    // as a reader holds them.
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

criterion_group!(benches, publication);
criterion_main!(benches);
