use super::{
    load_test_vectors_and_sync_chain_index, load_test_vectors_and_sync_chain_index_with_timings,
    MockchainMode,
};
use crate::chain_index::{ChainIndex, SyncTimings};
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

/// The start-up failure budget must exceed the steady-state one, or #1006's fix
/// is a no-op — the loop would give up on a validator that is still booting just
/// as quickly as on one that has genuinely broken.
///
/// Synchronous: the body is pure arithmetic over the timing constants.
#[test]
fn startup_budget_outlasts_steady_state_budget() {
    for (label, timings) in [
        ("production", SyncTimings::default()),
        ("fast", SyncTimings::fast()),
    ] {
        assert!(
            timings.max_startup_backoff_window() > timings.max_backoff_window(),
            "{label} start-up window ({:?}) must outlast the steady-state window ({:?})",
            timings.max_startup_backoff_window(),
            timings.max_backoff_window(),
        );
    }
}

/// Regression test for #1006: a validator that is not yet usable when zaino
/// starts must be waited out, not treated as a broken one.
///
/// A node that has not committed genesis reports no best block height, which
/// reaches the sync loop as the same `ErrorFromSource` a broken node produces.
/// The loop cannot tell them apart, so before the fix the steady-state budget
/// (~40 s in production) applied to start-up: the worker gave up and `return`ed,
/// and a validator that came up afterwards could never revive it. An external
/// pre-start delay narrowed that window but could not close it.
///
/// The loop now spends `startup_max_consecutive_failures` before its first
/// successful iteration, so it outlives the steady-state window and picks the
/// chain up once the validator arrives.
///
/// `multi_thread` required: the assertions depend on the spawned sync worker
/// running concurrently with this future's sleeps.
#[tokio::test(flavor = "multi_thread")]
async fn waits_out_validator_that_is_unavailable_at_startup() {
    let timings = SyncTimings::fast();
    let blocks = super::load_test_vectors().unwrap().blocks;
    let mockchain = super::build_active_mockchain_source(150, blocks);

    // Unavailable before construction begins, so the very first request the
    // index makes observes an unusable validator rather than racing the flag.
    mockchain.set_failing(true);

    let construction = tokio::spawn({
        let source = mockchain.clone();
        async move { super::build_index_with_source(source, MockchainMode::Active, timings).await }
    });

    // Before the fix, construction failed here in well under a second with
    // `MempoolError::Critical("Error connecting with validator")`. Outlast the
    // steady-state budget to show it is now waiting rather than giving up.
    sleep(timings.max_backoff_window()).await;
    assert!(
        !construction.is_finished(),
        "chain-index construction gave up on a validator that was merely not \
         ready yet; it must wait for the chain to come online (#1006)"
    );

    // The validator commits its genesis block and starts serving.
    mockchain.set_failing(false);
    let expected_tip = mockchain.active_height();

    let (_indexer, index_reader) = construction.await.expect("construction task panicked");

    super::poll::poll_until(
        "indexer to sync once the validator became available",
        Duration::from_secs(30),
        timings.initial_backoff,
        || async {
            let tip = index_reader
                .snapshot_nonfinalized_state()
                .await
                .ok()?
                .get_nfs_snapshot()?
                .best_tip
                .height
                .0;
            (tip == expected_tip).then_some(())
        },
    )
    .await;

    assert_eq!(
        index_reader.status(),
        StatusType::Ready,
        "sync loop should report Ready once it has caught up with the validator"
    );
}

/// Reproduction for #1006 — the failure is terminal, not transient.
///
/// When the backoff ladder is exhausted the sync loop does not merely report
/// [`StatusType::CriticalError`]: it `return`s, so the worker task itself is
/// gone. A validator that becomes reachable afterwards can never revive it.
///
/// This is what makes "zaino assumes the genesis block is confirmed" a race
/// rather than a slow start. A validator that has not yet committed genesis
/// makes `get_best_block_height` yield nothing, every iteration counts as a
/// failure, and ~40 s later (production timings) the worker exits for good.
/// At startup that is fatal: `NodeBackedIndexerService::launch` polls the
/// status and aborts the daemon on `CriticalError`. An external pre-start
/// delay narrows the window but cannot close it.
///
/// `multi_thread` required: the assertions depend on the spawned sync worker
/// running concurrently with this future's sleeps.
#[tokio::test(flavor = "multi_thread")]
async fn critical_error_is_terminal_and_source_recovery_does_not_revive_sync() {
    let timings = SyncTimings::fast();
    let (_blocks, _indexer, index_reader, mockchain) =
        load_test_vectors_and_sync_chain_index_with_timings(MockchainMode::Active, timings).await;

    let index_tip = || async {
        index_reader
            .snapshot_nonfinalized_state()
            .await
            .ok()
            .and_then(|s| s.get_nfs_snapshot().map(|n| n.best_tip.height.0))
    };

    // The validator becomes unavailable. At startup this is the window in
    // which it has not yet committed genesis.
    mockchain.set_failing(true);

    let max_wait = timings.max_backoff_window() * 5;
    let start = Instant::now();
    loop {
        sleep(timings.initial_backoff).await;
        if index_reader.status() == StatusType::CriticalError {
            break;
        }
        assert!(
            start.elapsed() < max_wait,
            "CriticalError was not reached within {max_wait:?}"
        );
    }
    let tip_at_failure = index_tip().await;

    // The validator comes back and extends the chain — the recovery an
    // operator (or an external start-up delay) would expect zaino to ride out.
    mockchain.set_failing(false);
    mockchain.mine_blocks(5);
    let recovered_source_tip = mockchain.active_height();

    // Give the loop far longer than a full backoff ladder to notice.
    sleep(max_wait).await;

    assert_eq!(
        index_reader.status(),
        StatusType::CriticalError,
        "sync loop recovered after the source returned — if this now passes, \
         the terminal-failure behaviour reported in #1006 has been fixed"
    );
    assert_eq!(
        index_tip().await,
        tip_at_failure,
        "index tip advanced to {recovered_source_tip} after recovery, so the \
         sync worker was still alive — #1006's premise no longer holds"
    );
}

/// Moved here from the integration test
/// `chain_cache::sync_large_chain_{zebrad,zcashd}`. That test contained one
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
            let tip = index_reader
                .snapshot_nonfinalized_state()
                .await
                .ok()?
                .get_nfs_snapshot()?
                .best_tip
                .height
                .0;
            (tip == expected_tip).then_some(())
        },
    )
    .await;

    let indexer_tip = index_reader
        .snapshot_nonfinalized_state()
        .await
        .unwrap()
        .get_nfs_snapshot()
        .unwrap()
        .best_tip
        .height
        .0;
    assert_eq!(indexer_tip, expected_tip);
}
