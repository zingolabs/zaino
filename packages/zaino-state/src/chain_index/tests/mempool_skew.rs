//! Regression suite for #1037 — mempool↔chain-index tip skew.
//!
//! The chain-index has two independent sync tasks observing the same
//! `BlockchainSource` on independent cadences: the chain-index sync loop
//! and the mempool serve loop. Their tip views can diverge for a window
//! after every block. This module pins each direction of the skew with a
//! deterministic test so the principled fix can be TDD'd against both.

use tokio::time::{timeout, Duration};

use super::{load_test_vectors_and_sync_chain_index, mockchain_tests::wait_for_indexer_tip};
use crate::{
    chain_index::{
        source::mockchain_source::MockchainSource, ChainIndex, NodeBackedChainIndexSubscriber,
    },
    BlockHash,
};

/// Waits for the next change of the mempool serve loop's `mempool_chain_tip`
/// watch — i.e. the next time the mempool observes a new best-block hash and
/// resets. Used by direction-#2 to act *exactly* in the window where the
/// mempool has advanced but the chain-index has not.
async fn wait_for_mempool_tip_change(
    index_reader: &NodeBackedChainIndexSubscriber<MockchainSource>,
) {
    let mut tip = index_reader.mempool_tip();
    tip.borrow_and_update();
    timeout(Duration::from_secs(10), tip.changed())
        .await
        .unwrap_or_else(|_| panic!("mempool tip did not change within 10 s"))
        .expect("mempool_chain_tip sender dropped");
}

/// Waits until the mempool serve loop's `mempool_chain_tip` equals
/// `expected`, or panics after 10 s. Used to re-synchronise between
/// iterations of property-style skew tests.
async fn wait_for_mempool_tip(
    index_reader: &NodeBackedChainIndexSubscriber<MockchainSource>,
    expected: BlockHash,
) {
    let mut tip = index_reader.mempool_tip();
    let work = async {
        loop {
            if *tip.borrow_and_update() == expected {
                return;
            }
            tip.changed()
                .await
                .expect("mempool_chain_tip sender dropped");
        }
    };
    timeout(Duration::from_secs(10), work)
        .await
        .unwrap_or_else(|_| panic!("mempool tip never reached expected hash within 10 s"));
}

/// Tip-skew direction #1 (#1037): chain-index sync loop ahead of mempool.
///
/// After `mine_blocks(1)` + `wait_for_indexer_tip`, the chain-index has
/// reached the new tip but the mempool's serve loop may not have. Calling
/// `get_mempool_stream` with a snapshot taken before mining must reject
/// the snapshot as stale even when the mempool's own tip view still
/// matches it — i.e. staleness must be defined against the *latest*
/// observed tip, not whichever subsystem the consumer happens to read
/// first. Companion direction (mempool ahead of chain-index) lives in
/// the sibling test.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn chain_index_ahead_returns_stale_stream() {
    let (_blocks, _indexer, index_reader, mockchain) =
        load_test_vectors_and_sync_chain_index(true).await;
    wait_for_indexer_tip(&index_reader, mockchain.active_height()).await;

    let stale_nonfinalized_snapshot = index_reader.snapshot_nonfinalized_state().await.unwrap();

    mockchain.mine_blocks(1);
    wait_for_indexer_tip(&index_reader, mockchain.active_height()).await;

    let mempool_stream = index_reader.get_mempool_stream(Some(&stale_nonfinalized_snapshot));

    assert!(mempool_stream.is_none());
}

/// Tip-skew direction #2 (#1037): mempool serve loop ahead of chain-index.
///
/// Constructed by advancing the mockchain via `mine_blocks_silent`, which
/// suppresses the source's `blocks_received_broadcaster` wake. The
/// mempool's serve loop polls `get_best_block_hash` on its own cadence,
/// so it observes the new tip first. Once the mempool has advanced
/// (`wait_for_mempool_tip_change`), the test takes a snapshot — which
/// reflects the chain-index's *current* (lagging) view — and asks for a
/// mempool stream against that snapshot.
///
/// Calling `get_mempool_stream(snapshot)` immediately after a
/// `snapshot_nonfinalized_state()` from the same
/// `NodeBackedChainIndexSubscriber` should not reject the caller's
/// snapshot: the API just handed it out, it is the freshest chain-index
/// view available, and refusing the stream for it is an internal
/// contradiction (left hand says fresh, right hand says stale). Today
/// the staleness check compares `snapshot.best_tip.hash` against
/// `mempool_chain_tip` only, so when the mempool has moved past the
/// chain-index, this comparison fails and the stream is dropped.
///
/// Expected to fail until #1037 is fixed.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn mempool_ahead_rejects_fresh_snapshot() {
    let (_blocks, _indexer, index_reader, mockchain) =
        load_test_vectors_and_sync_chain_index(true).await;
    wait_for_indexer_tip(&index_reader, mockchain.active_height()).await;

    // Advance the source without notifying the chain-index sync loop,
    // letting the mempool's poll observe the new tip first.
    mockchain.mine_blocks_silent(1);
    wait_for_mempool_tip_change(&index_reader).await;

    // The snapshot the API just handed out should yield a valid stream.
    let fresh_snapshot = index_reader.snapshot_nonfinalized_state().await.unwrap();

    // Sanity check: chain-index is still at the old tip in this snapshot.
    let chain_index_tip = fresh_snapshot.get_nfs_snapshot().unwrap().best_tip.height.0;
    assert!(
        chain_index_tip < mockchain.active_height(),
        "test setup: chain-index should be behind the mockchain (got chain_index={chain_index_tip}, mockchain={})",
        mockchain.active_height(),
    );

    let mempool_stream = index_reader.get_mempool_stream(Some(&fresh_snapshot));

    assert!(
        mempool_stream.is_some(),
        "API rejected its own freshly-issued snapshot — mempool↔chain-index tip skew (#1037 direction #2)"
    );
}

/// Convergence-bound (#1037 success criterion): each `mine_blocks` event
/// should bring both subsystems to the new tip within a single iteration.
///
/// At each iteration the test mines one block, awaits the chain-index
/// tip via the existing event-driven helper, then samples the mempool's
/// tracked tip *immediately*. If the mempool also converged in the same
/// iteration, the sample reflects the new tip; otherwise the sample
/// still equals the pre-mining hash and the iteration is counted as a
/// lag.
///
/// Today the chain-index sync loop wakes on `mine_blocks` (Option-2
/// notify) but the mempool serve loop polls on its own 100 ms cadence,
/// so the chain-index nearly always wins the race and the mempool lags
/// in the vast majority of iterations. Repeating the sample 20× makes
/// the failure deterministic in practice — the probability of zero lags
/// across 20 events under a ~10 % per-iteration win rate is negligible.
///
/// Once the principled fix wakes both subsystems on `mine_blocks`, the
/// expected outcome is `lag_count == 0` deterministically.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn tips_converge_within_bounded_time_after_mining() {
    let (_blocks, _indexer, index_reader, mockchain) =
        load_test_vectors_and_sync_chain_index(true).await;
    wait_for_indexer_tip(&index_reader, mockchain.active_height()).await;

    let mut lag_count = 0;
    for _ in 0..20 {
        let pre_mempool_tip = *index_reader.mempool_tip().borrow();
        mockchain.mine_blocks(1);
        let new_height = mockchain.active_height();
        let new_hash = mockchain.active_block_hash();

        wait_for_indexer_tip(&index_reader, new_height).await;
        if *index_reader.mempool_tip().borrow() == pre_mempool_tip {
            lag_count += 1;
        }

        // Resync mempool before the next iteration so we're not
        // chasing multiple stacked updates.
        wait_for_mempool_tip(&index_reader, new_hash).await;
    }

    assert_eq!(
        lag_count, 0,
        "mempool lagged chain-index in {lag_count}/20 mine_blocks events (#1037)"
    );
}
