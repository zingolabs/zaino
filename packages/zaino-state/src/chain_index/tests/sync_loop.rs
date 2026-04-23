use super::{
    load_test_vectors_and_sync_chain_index, load_test_vectors_and_sync_chain_index_with_timings,
};
use crate::chain_index::SyncTimings;
use std::time::Instant;
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
    let max_time_to_critical = SyncTimings::default().max_backoff_window() + Duration::from_secs(5);
    assert!(
        elapsed < max_time_to_critical,
        "test took {elapsed:?}, which exceeds the maximum possible backoff window"
    );
}

/// After `max_consecutive_failures` with exponential backoff, the sync loop
/// should escalate to [`StatusType::CriticalError`].
///
/// Uses [`SyncTimings::fast`] (10× shrunk) so the full backoff schedule fits
/// in a few seconds instead of ~40 s.
#[tokio::test(flavor = "multi_thread")]
async fn escalates_to_critical_after_persistent_failure() {
    let timings = SyncTimings::fast();
    let (_blocks, _indexer, index_reader, mockchain) =
        load_test_vectors_and_sync_chain_index_with_timings(true, timings).await;

    let start = Instant::now();
    mockchain.set_failing(true);

    // 5× slack over the nominal backoff sum to absorb scheduling jitter and
    // the per-iteration sync work the loop performs between sleeps.
    let max_time_to_critical = timings.max_backoff_window() * 5;
    let poll_interval = timings.initial_backoff;

    loop {
        sleep(poll_interval).await;

        if index_reader.status() == StatusType::CriticalError {
            break;
        }

        assert!(
            start.elapsed() < max_time_to_critical,
            "CriticalError was not reached within {max_time_to_critical:?}"
        );
    }

    let elapsed = start.elapsed();
    assert!(
        elapsed < max_time_to_critical,
        "CriticalError took {elapsed:?}, exceeding the maximum backoff window"
    );
}
