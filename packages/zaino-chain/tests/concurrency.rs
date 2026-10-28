//! The bounds that keep thousands of clients from overwhelming one validator.
//!
//! These properties are invisible in every return value — a view that ignored
//! its permit pool would pass the rest of the suite and fall over only under
//! load, which is the failure mode worth a dedicated file.

use std::sync::Arc;

use futures::TryStreamExt as _;
use zaino_chain::testing::{height, Chain, FakeHead, FakeSource, FakeStore};
use zaino_chain::{ChainViewComposer, ChainViewConfig, CompactBlockRead as _};
use zaino_chain_store::PoolFilter;

/// A view whose whole range must be filled from the validator.
fn all_from_validator(
    chain: &Chain,
    config: ChainViewConfig,
) -> ChainViewComposer<FakeStore, FakeHead, FakeSource> {
    ChainViewComposer::new(
        FakeStore::empty(),
        FakeHead::covering(chain, 1100, 1200),
        Arc::new(FakeSource::over(chain)),
        config.without_store(),
    )
}

/// The shared pool bounds validator requests however large the range is.
///
/// A compact fill issues two requests per block — the projection and its tree
/// sizes — and they are deliberately issued together, so the ceiling is twice
/// the permit count. Anything above that means the pool is not being taken.
#[tokio::test(flavor = "multi_thread")]
async fn the_permit_pool_bounds_validator_concurrency() {
    let chain = Chain::of_length(1201);
    let source = FakeSource::over(&chain);
    let permits = 4;

    let composer = ChainViewComposer::new(
        FakeStore::empty(),
        FakeHead::covering(&chain, 1100, 1200),
        Arc::new(source.clone()),
        ChainViewConfig::default()
            .without_store()
            .with_passthrough_permits(permits)
            .with_passthrough_per_request(64),
    );

    let blocks: Vec<_> = composer
        .snapshot()
        .stream_compact(height(0), height(400), PoolFilter::all())
        .try_collect::<Vec<_>>()
        .await
        .expect("serviceable")
        .into_iter()
        .flatten()
        .collect();

    assert_eq!(blocks.len(), 401, "the whole range should be filled");
    // Proves the bound is doing something: without real overlap the ceiling
    // would hold vacuously, and this test would pass on a serial implementation.
    assert!(
        source.peak_in_flight() > 1,
        "fills did not actually overlap, so the bound is untested"
    );
    assert!(
        source.peak_in_flight() <= permits * 2,
        "peak in-flight {} exceeded {} permits (x2 requests per block)",
        source.peak_in_flight(),
        permits * 2
    );
}

/// One large request cannot occupy the whole pool.
///
/// The fairness bound beneath the shared one: without it a single client
/// streaming a long range would hold every permit and stall everyone behind it.
#[tokio::test(flavor = "multi_thread")]
async fn a_single_request_is_bounded_below_the_pool() {
    let chain = Chain::of_length(1201);
    let source = FakeSource::over(&chain);

    let composer = ChainViewComposer::new(
        FakeStore::empty(),
        FakeHead::covering(&chain, 1100, 1200),
        Arc::new(source.clone()),
        ChainViewConfig::default()
            .without_store()
            .with_passthrough_permits(256)
            .with_passthrough_per_request(2),
    );

    let _ = composer
        .snapshot()
        .stream_compact(height(0), height(200), PoolFilter::all())
        .try_collect::<Vec<Vec<_>>>()
        .await
        .expect("serviceable");

    assert!(
        source.peak_in_flight() <= 4,
        "one request reached {} in flight against a per-request cap of 2",
        source.peak_in_flight()
    );
}

/// Many clients sharing a view share its pool.
///
/// The property the bound exists for: concurrency is a function of the view,
/// not of the number of clients.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_clients_share_one_bound() {
    let chain = Chain::of_length(1201);
    let source = FakeSource::over(&chain);
    let permits = 8;

    let composer = Arc::new(ChainViewComposer::new(
        FakeStore::empty(),
        FakeHead::covering(&chain, 1100, 1200),
        Arc::new(source.clone()),
        ChainViewConfig::default()
            .without_store()
            .with_passthrough_permits(permits)
            .with_passthrough_per_request(8),
    ));

    let clients: Vec<_> = (0..32)
        .map(|client| {
            let composer = Arc::clone(&composer);
            tokio::spawn(async move {
                let start = client * 10;
                composer
                    .snapshot()
                    .stream_compact(height(start), height(start + 40), PoolFilter::all())
                    .try_collect::<Vec<Vec<_>>>()
                    .await
                    .map(|chunks| chunks.into_iter().flatten().count())
            })
        })
        .collect();

    for client in clients {
        let served = client
            .await
            .expect("client task")
            .expect("every client is serviceable");
        assert_eq!(served, 41);
    }

    assert!(
        source.peak_in_flight() <= permits * 2,
        "32 clients reached {} in flight against {} permits",
        source.peak_in_flight(),
        permits
    );
}

/// The store path is never gated by the validator bound.
///
/// The normal path once a deployment has caught up. A permit pool that also
/// throttled store reads would cap the throughput of the case that should be
/// fastest.
#[tokio::test(flavor = "multi_thread")]
async fn store_reads_take_no_permits() {
    let chain = Chain::of_length(1201);
    let source = FakeSource::over(&chain);

    let composer = ChainViewComposer::new(
        FakeStore::covering(&chain, 1200),
        FakeHead::covering(&chain, 1150, 1200),
        Arc::new(source.clone()),
        // One permit: if a store read needed one, a range would serialise
        // behind it, and if it needed one that was never released it would
        // deadlock.
        ChainViewConfig::default().with_passthrough_permits(1),
    );

    let blocks: Vec<_> = composer
        .snapshot()
        .stream_compact(height(0), height(1000), PoolFilter::all())
        .try_collect::<Vec<_>>()
        .await
        .expect("serviceable")
        .into_iter()
        .flatten()
        .collect();

    assert_eq!(blocks.len(), 1001);
    assert_eq!(
        source.peak_in_flight(),
        0,
        "the validator was consulted for a range the store covers"
    );
}

/// A view with no store still serves, and stays bounded.
#[tokio::test(flavor = "multi_thread")]
async fn a_validator_only_view_serves_within_its_bound() {
    let chain = Chain::of_length(1201);
    let composer = all_from_validator(&chain, ChainViewConfig::default());

    let blocks: Vec<_> = composer
        .snapshot()
        .stream_compact(height(0), height(300), PoolFilter::all())
        .try_collect::<Vec<_>>()
        .await
        .expect("serviceable")
        .into_iter()
        .flatten()
        .collect();

    assert_eq!(blocks.len(), 301);
}
