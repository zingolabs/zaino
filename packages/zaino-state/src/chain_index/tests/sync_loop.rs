use std::num::{NonZeroU32, NonZeroU64};
use std::time::Instant;

use tokio::time::{sleep, Duration};
use zaino_chain_head::ChainHeadConfig;
use zaino_status::{Status as _, StatusType};

use super::{
    load_test_vectors_and_sync_chain_index,
    load_test_vectors_and_sync_chain_index_with_chain_head_config, MockchainMode,
};
use crate::chain_index::chain_view::BestTip as _;
use crate::chain_index::{chain_head, combine_component_statuses, ChainIndex};

/// The operational chain head config with a failure ladder short enough to
/// run to the end in a test.
fn fast_chain_head_config() -> ChainHeadConfig {
    let ms = |millis| NonZeroU64::new(millis).expect("test interval is not zero");
    let mut config = chain_head::config();
    config.set_poll_interval_ms(ms(50));
    config.set_initial_backoff_ms(ms(25));
    config.set_max_backoff_ms(ms(800));
    config.set_max_consecutive_failures(NonZeroU32::new(10).expect("10 is not zero"));
    config
}

/// The total backoff the chain head sleeps before giving up.
fn max_backoff_window(config: &ChainHeadConfig) -> Duration {
    let mut total = Duration::ZERO;
    let mut current = config.initial_backoff();
    for _ in 1..config.max_consecutive_failures() {
        total += current;
        current = (current * 2).min(config.max_backoff());
    }
    total
}

/// Regression test (fixes #593): a transient source failure must not escalate
/// the index to `CriticalError`, which would stop the server.
#[tokio::test(flavor = "multi_thread")]
async fn survives_transient_source_failure() {
    let (_blocks, _indexer, index_reader, mockchain) =
        load_test_vectors_and_sync_chain_index(MockchainMode::Active).await;

    mockchain.source().set_failing(true);
    sleep(Duration::from_secs(2)).await;

    assert_ne!(
        index_reader.status(),
        StatusType::CriticalError,
        "a transient source failure must not escalate to CriticalError"
    );
}

/// After `max_consecutive_failures` with exponential backoff, the chain head
/// gives up and the index reports [`StatusType::CriticalError`].
#[tokio::test(flavor = "multi_thread")]
async fn escalates_to_critical_after_persistent_failure() {
    let config = fast_chain_head_config();
    let (_blocks, _indexer, index_reader, mockchain) =
        load_test_vectors_and_sync_chain_index_with_chain_head_config(
            MockchainMode::Active,
            config.clone(),
        )
        .await;

    let start = Instant::now();
    mockchain.source().set_failing(true);

    // 5× slack over the nominal backoff sum to absorb scheduling jitter.
    let max_time_to_critical = max_backoff_window(&config) * 5;
    let poll_interval = config.initial_backoff();

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
