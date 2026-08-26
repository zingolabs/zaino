//! Zaino-State ChainIndex Finalised State (FinalisedState) unit tests.
pub(crate) mod ephemeral;
mod migrations;
mod ports;
pub(crate) mod v1;

use std::future::Future;
use tempfile::TempDir;

use crate::error::StoreError;
use crate::store::FinalisedState;
use crate::tests::fixtures::FakeValidator;
use crate::tests::fixtures::{fake_validator_from_vectors, load_test_vectors};
use crate::tests::init_tracing;
use zaino_chain_store::ChainStoreSource;

/// Regression helper for zingolabs/zaino#1032.
///
/// Spawns a `FinalisedState` with the provided version-specific spawner, waits for
/// ready, then asserts that `shutdown()` returns in well under 5 s — i.e. the
/// background handle is awaited, not padded with an unconditional sleep.
async fn assert_shutdown_returns_promptly<F, Fut, T>(version_label: &str, spawn_fn: F)
where
    F: FnOnce(std::sync::Arc<FakeValidator>) -> Fut,
    Fut: Future<Output = Result<(TempDir, FinalisedState<T>), StoreError>>,
    T: ChainStoreSource,
{
    init_tracing();

    let source = fake_validator_from_vectors(&load_test_vectors().unwrap().blocks);
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
