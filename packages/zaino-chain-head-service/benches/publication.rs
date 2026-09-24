//! What the ChainHead costs: publishing, and answering.
//!
//! The chain head's per-tick work is dominated by the snapshot it builds: it
//! copies the published one, extends it, trims it, and publishes the result,
//! while readers keep earlier ones alive. This measures that path through the
//! service, so a change to how the snapshot is stored shows up here. It also
//! measures the queries a published snapshot answers, which is the other half
//! of that trade: a representation that publishes cheaply may read slower.
//!
//! Everything here goes through the crate's public API, which is what a
//! benchmark target can reach. The graph's own moves are crate-private, so
//! they are driven through the service rather than directly.
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

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use harness::{anchored, hold_publications, synced, MockValidator, REORG_DEPTH, WINDOW};
use tokio::runtime::Runtime;
use zaino_chain_head::{ChainHeadBlockService as _, ChainHeadSnapshot as _};

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
            |(validator, service)| hold_publications(&runtime, &validator, &service),
            BatchSize::LargeInput,
        );
    });

    // A reorg: the validator replaces the last blocks with a competing branch,
    // so the advance rewinds to the fork point and extends over it.
    group.bench_function("reorg 10 blocks", |bencher| {
        bencher.iter_batched(
            || {
                let validator = MockValidator::linear(WINDOW);
                let service = synced(&runtime, &validator);
                (validator, service)
            },
            |(validator, service)| {
                validator.reorg(WINDOW - REORG_DEPTH, REORG_DEPTH);
                runtime.block_on(async { service.advance_once().await.expect("advance succeeds") });
            },
            BatchSize::LargeInput,
        );
    });

    group.finish();
}

/// What a published snapshot costs to answer from: the other half of the
/// trade, since a representation that publishes cheaply may read slower.
fn reads(criterion: &mut Criterion) {
    let runtime = Runtime::new().expect("a tokio runtime");
    let validator = MockValidator::linear(WINDOW);
    let service = synced(&runtime, &validator);
    let snapshot = service.subscriber().current();
    let hashes: Vec<_> = snapshot.best_chain().map(|block| block.hash()).collect();

    let mut group = criterion.benchmark_group("chain-head-reads");
    group.sample_size(50);

    group.bench_function("best chain walk", |bencher| {
        bencher.iter(|| {
            snapshot
                .best_chain()
                .map(|block| u64::from(u32::from(block.height())))
                .sum::<u64>()
        });
    });

    group.bench_function("lookup every block", |bencher| {
        bencher.iter(|| {
            hashes
                .iter()
                .filter(|hash| snapshot.block_by_hash(hash).is_some())
                .count()
        });
    });

    group.bench_function("chain tips", |bencher| {
        bencher.iter(|| snapshot.chain_tips().len());
    });

    group.finish();
}

criterion_group!(benches, publication, reads);
criterion_main!(benches);
