use super::{
    load_test_vectors_and_sync_chain_index, load_test_vectors_and_sync_chain_index_with_timings,
    MockchainMode,
};
use crate::chain_index::{combine_component_statuses, ChainIndex, SyncTimings};
use std::time::Instant;
use tokio::time::{sleep, Duration};
use zaino_chain_head::ChainHeadSnapshot as _;
use zaino_status::{Status as _, StatusType};

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
        load_test_vectors_and_sync_chain_index(MockchainMode::Active).await;

    let start = Instant::now();
    mockchain.source().set_failing(true);
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
        load_test_vectors_and_sync_chain_index_with_timings(MockchainMode::Active, timings).await;

    let start = Instant::now();
    mockchain.source().set_failing(true);

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

/// The chain head is one of the components the status fold accounts for.
///
/// ChainHead synchronises itself, so nothing else reports on its behalf: if it
/// is left out of the fold, a head that has given up on the validator serves a
/// frozen tip while the index still reports `Ready`. Asserting on the fold
/// directly pins *which* components are accounted for, which the integration
/// tests below cannot — they cannot fail a single component in isolation.
#[test]
fn the_fold_accounts_for_every_component() {
    let ready = StatusType::Ready;

    assert_eq!(
        combine_component_statuses(ready, ready, ready, StatusType::CriticalError),
        StatusType::CriticalError,
        "a chain head that has given up must not be reported as Ready"
    );
    assert_eq!(
        combine_component_statuses(ready, ready, ready, StatusType::RecoverableError),
        StatusType::RecoverableError,
    );
    assert_eq!(
        combine_component_statuses(ready, StatusType::Syncing, ready, ready),
        StatusType::Syncing,
    );
    assert_eq!(
        combine_component_statuses(ready, ready, StatusType::RecoverableError, ready),
        StatusType::RecoverableError,
    );
    assert_eq!(
        combine_component_statuses(ready, ready, ready, ready),
        ready,
        "all components healthy is the only way to report Ready"
    );
}

/// Component failures are reported while they last, and stop being reported
/// once the component recovers.
///
/// The fold used to write its result back into the index's own status cell,
/// which latched: the first transient failure pinned the index to
/// `RecoverableError` — and `is_ready()` to false — for the rest of the
/// process's life. That mattered little while the fold covered only the
/// finalised state and the mempool; the chain head enters `RecoverableError`
/// on any transient validator blip, so a latch would make a single blip
/// permanent.
#[tokio::test(flavor = "multi_thread")]
async fn status_recovers_after_a_transient_source_failure() {
    let (_blocks, _indexer, index_reader, mockchain) =
        load_test_vectors_and_sync_chain_index(MockchainMode::Active).await;

    mockchain.source().set_failing(true);
    super::poll::poll_until(
        "the index to report a component failure",
        Duration::from_secs(10),
        Duration::from_millis(25),
        || async { (index_reader.status() == StatusType::RecoverableError).then_some(()) },
    )
    .await;

    mockchain.source().set_failing(false);

    // Generous budget: each failing component is on its own backoff ladder, and
    // the chain head's doubles from 500 ms, so the last one to notice the
    // source is healthy again can be several seconds behind the first.
    super::poll::poll_until(
        "the index to report Ready again",
        Duration::from_secs(30),
        Duration::from_millis(50),
        || async { (index_reader.status() == StatusType::Ready).then_some(()) },
    )
    .await;
}

/// Moved here from the integration test
/// `chain_cache::sync_large_chain_zebrad`. That test contained one
/// whitebox read — `snapshot.best_tip.height` (W11 in the issue #1044
/// audit) — asserting the indexer tip matched the validator tip after
/// ~150 blocks were produced in a burst. That property is about the sync
/// loop absorbing many new source blocks between iterations, not about
/// chain-cache shape, so it belongs next to the other sync-loop tests
/// and inside the crate where the snapshot's fields are reachable.
///
/// `sync_blocks_after_startup` covers the one-block-at-a-time trickle.
/// This test covers the distinct case where multiple blocks appear on
/// the source before the next sync iteration runs. Porting to
/// `MockSource` (which implements `BlockchainReader`) keeps the
/// indexer's production sync code in the loop while removing the podman
/// / live-validator fixture dependency the original test required.
#[tokio::test(flavor = "multi_thread")]
async fn tip_converges_after_burst_mine() {
    let (_blocks, _indexer, index_reader, mockchain) =
        load_test_vectors_and_sync_chain_index(MockchainMode::Active).await;

    let initial_tip = mockchain.source().active_height();
    mockchain.source().mine_blocks(20);
    let expected_tip = mockchain.source().active_height();
    assert!(
        expected_tip > initial_tip,
        "mockchain did not advance: burst mine was a no-op \
         (initial_tip={initial_tip}, max_chain_height={})",
        mockchain.source().max_chain_height(),
    );

    super::poll::poll_until(
        "indexer tip to match mined mockchain tip",
        Duration::from_secs(10),
        Duration::from_millis(25),
        || async {
            let tip = u32::from(index_reader.snapshot_nonfinalized_state().best_tip().height);
            (tip == expected_tip).then_some(())
        },
    )
    .await;

    let indexer_tip = u32::from(index_reader.snapshot_nonfinalized_state().best_tip().height);
    assert_eq!(indexer_tip, expected_tip);
}
