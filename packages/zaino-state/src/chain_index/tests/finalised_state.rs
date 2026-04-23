//! Zaino-State ChainIndex Finalised State (ZainoDB) unit tests.
mod migrations;
mod v0;
pub(crate) mod v1;

use std::future::Future;
use tempfile::TempDir;

use crate::chain_index::finalised_state::ZainoDB;
use crate::chain_index::source::test::MockchainSource;
use crate::chain_index::tests::init_tracing;
use crate::chain_index::tests::vectors::{build_mockchain_source, load_test_vectors};
use crate::error::FinalisedStateError;

/// Regression helper for zingolabs/zaino#1032.
///
/// Spawns a `ZainoDB` with the provided version-specific spawner, waits for
/// ready, then asserts that `shutdown()` returns in well under 5 s — i.e. the
/// background handle is awaited, not padded with an unconditional sleep.
async fn assert_shutdown_returns_promptly<F, Fut>(version_label: &str, spawn_fn: F)
where
    F: FnOnce(MockchainSource) -> Fut,
    Fut: Future<Output = Result<(TempDir, ZainoDB), FinalisedStateError>>,
{
    init_tracing();

    let source = build_mockchain_source(load_test_vectors().unwrap().blocks);
    let (_db_dir, zaino_db) = spawn_fn(source).await.unwrap();
    zaino_db.wait_until_ready().await;

    let start = std::time::Instant::now();
    zaino_db.shutdown().await.unwrap();
    let elapsed = start.elapsed();

    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "{version_label} shutdown took {elapsed:?}, expected < 1 s"
    );
}
