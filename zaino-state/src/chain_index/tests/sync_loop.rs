use super::load_test_vectors_and_sync_chain_index;
use tokio::time::{sleep, Duration};
use zaino_common::status::{Status as _, StatusType};

/// Regression test (fixes #593): a source failure should not kill the
/// sync loop.
///
/// The sync loop (chain_index.rs) sleeps 500ms between iterations. On
/// failure, it used to propagate via `?` and set CriticalError. The
/// indexer serve loop (indexer.rs) checks status every 100ms — so within
/// 100ms of the sync loop failing it called close(), dropping the
/// TonicServer. Integration test clients then got ConnectionRefused
/// because the gRPC port was never reachable.
///
/// The sync loop now retries with exponential backoff and remains live.
#[tokio::test(flavor = "multi_thread")]
async fn survives_transient_source_failure() {
    let (_blocks, _indexer, index_reader, mockchain) =
        load_test_vectors_and_sync_chain_index(true).await;

    mockchain.set_failing(true);
    sleep(Duration::from_secs(2)).await;

    assert_ne!(
        index_reader.status(),
        StatusType::CriticalError,
        "sync loop should survive transient source failure, not set CriticalError"
    );
}
