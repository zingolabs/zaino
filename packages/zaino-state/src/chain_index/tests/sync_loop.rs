use super::load_test_vectors_and_sync_chain_index;
use std::time::Instant;
use tokio::time::{sleep, Duration};
use zaino_common::status::{Status as _, StatusType};

// The backoff sequence is: 250ms, 500ms, 1s, 2s, 4s, 8s, 8s, 8s, 8s
// then CriticalError on the 10th failure.
// Total: ~40s of backoff + up to 500ms until the sync loop's next iteration.
const MAX_TIME_TO_CRITICAL: Duration = Duration::from_secs(45);

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

    let start = Instant::now();
    mockchain.set_failing(true);
    sleep(Duration::from_secs(2)).await;

    let status = index_reader.status();
    let elapsed = start.elapsed();

    assert_ne!(
        status,
        StatusType::CriticalError,
        "sync loop should survive transient source failure, not set CriticalError"
    );
    assert!(
        elapsed < MAX_TIME_TO_CRITICAL,
        "test took {elapsed:?}, which exceeds the maximum possible backoff window"
    );
}

/// After MAX_CONSECUTIVE_FAILURES (10) with exponential backoff
/// (250ms, 500ms, 1s, 2s, 4s, 8s, 8s, 8s, 8s = ~40s total),
/// the sync loop should escalate to CriticalError.
#[tokio::test(flavor = "multi_thread")]
async fn escalates_to_critical_after_persistent_failure() {
    let (_blocks, _indexer, index_reader, mockchain) =
        load_test_vectors_and_sync_chain_index(true).await;

    let start = Instant::now();
    mockchain.set_failing(true);

    // Poll until CriticalError rather than sleeping a fixed duration.
    loop {
        sleep(Duration::from_secs(1)).await;

        if index_reader.status() == StatusType::CriticalError {
            break;
        }

        assert!(
            start.elapsed() < MAX_TIME_TO_CRITICAL,
            "CriticalError was not reached within {MAX_TIME_TO_CRITICAL:?}"
        );
    }

    let elapsed = start.elapsed();
    assert!(
        elapsed < MAX_TIME_TO_CRITICAL,
        "CriticalError took {elapsed:?}, exceeding the maximum backoff window"
    );
}
