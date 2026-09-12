//! ChainIndex's integration with the chain head.
//!
//! The chain head's own behaviour — retention, reorgs, competing branches,
//! publication — is tested in `zaino-chain-head-service` against a mock
//! validator. What is left to test here is the part only ChainIndex can be
//! wrong about: that the two layers it composes meet, and that its lifecycle
//! takes the chain head down with it.
//!
//! Several tests that used to live here pinned invariants of the old
//! non-finalised state that no longer have a referent. The seam is no longer
//! defined by the finalised DB's height — the chain head derives its window
//! from the chain tip and never reads the finalised state at all — so
//! "the lowest retained block equals the finalised tip" is not something either
//! layer now promises. What the two still owe jointly is coverage without a
//! gap, which is what `layers_meet_without_a_gap` checks.

use std::time::Duration;

use futures::StreamExt as _;
use zaino_chain_head::ChainHeadSnapshot as _;

use super::{load_test_vectors_and_sync_chain_index, poll::poll_until, MockchainMode};
use crate::chain_index::ChainIndex;

/// The chain head reaches the validator's tip, and ChainIndex serves it.
#[tokio::test(flavor = "multi_thread")]
async fn chain_head_reaches_the_validator_tip() {
    let (_blocks, _indexer, index_reader, mockchain) =
        load_test_vectors_and_sync_chain_index(MockchainMode::Active).await;

    let target_tip = mockchain.source().active_height();
    poll_until(
        "chain index to serve the validator's tip",
        Duration::from_secs(10),
        Duration::from_millis(50),
        || async {
            let tip = index_reader.snapshot_nonfinalized_state().best_tip();
            (u32::from(tip.height) == target_tip).then_some(())
        },
    )
    .await;
}

/// Every announced tip must already resolve through the same local index view.
#[tokio::test(flavor = "multi_thread")]
async fn indexed_tip_events_name_index_readable_blocks() {
    // Given
    let (_blocks, _indexer, index_reader, mockchain) =
        load_test_vectors_and_sync_chain_index(MockchainMode::Active).await;
    let mut tips = index_reader.indexed_tip_stream();
    let initial = tips.next().await.expect("the stream starts with a tip");

    // When
    mockchain.source().mine_blocks(1);
    let updated = tokio::time::timeout(Duration::from_secs(10), tips.next())
        .await
        .expect("the updated tip arrives before the deadline")
        .expect("the index remains available");

    // Then
    for tip in [initial, updated] {
        let snapshot = index_reader.snapshot_nonfinalized_state();
        let indexed_hash = index_reader
            .get_block_hash(&snapshot, crate::Height(u32::from(tip.height)))
            .await
            .expect("indexed block lookup succeeds")
            .expect("announced tip is readable");
        assert_eq!(indexed_hash.0, <[u8; 32]>::from(tip.hash));
    }
}

/// The finalised state and the chain head must jointly cover the chain with no
/// hole between them.
///
/// They synchronise independently now, so nothing makes this true by
/// construction — it holds because both derive the seam from the same chain tip
/// and the same depth. A regression in either one's floor calculation opens a
/// range of heights that neither layer will answer for, and that is invisible
/// until a client asks for exactly those blocks.
#[tokio::test(flavor = "multi_thread")]
async fn layers_meet_without_a_gap() {
    let (_blocks, _indexer, index_reader, mockchain) =
        load_test_vectors_and_sync_chain_index(MockchainMode::Active).await;

    let target_tip = mockchain.source().active_height();
    poll_until(
        "chain index to serve the validator's tip",
        Duration::from_secs(10),
        Duration::from_millis(50),
        || async {
            let tip = index_reader.snapshot_nonfinalized_state().best_tip();
            (u32::from(tip.height) == target_tip).then_some(())
        },
    )
    .await;

    let snapshot = index_reader.snapshot_nonfinalized_state();
    let lowest_retained = snapshot
        .best_chain()
        .next()
        .expect("the chain head always retains at least its tip")
        .height();

    // Every height from the lowest retained block up to the tip is answerable
    // from the chain head, and everything below it from the finalised state.
    // Probing the boundary is what would catch an off-by-one in either floor.
    for height in u32::from(lowest_retained)..=target_tip {
        assert!(
            index_reader
                .get_block_hash(&snapshot, crate::Height(height))
                .await
                .expect("block hash lookup succeeds")
                .is_some(),
            "no layer answers for height {height}",
        );
    }

    if u32::from(lowest_retained) > 0 {
        let below = crate::Height(u32::from(lowest_retained) - 1);
        assert!(
            index_reader
                .get_block_hash(&snapshot, below)
                .await
                .expect("block hash lookup succeeds")
                .is_some(),
            "the height just below the chain head window must come from the \
             finalised state, but nothing answered for {below:?}",
        );
    }
}

/// `shutdown()` stops the sync loop, and it exits without reporting an error.
#[tokio::test(flavor = "multi_thread")]
async fn shutdown_terminates_sync_loop_cleanly() {
    let (_blocks, mut indexer, index_reader, mockchain) =
        load_test_vectors_and_sync_chain_index(MockchainMode::Active).await;

    let target_tip = mockchain.source().active_height();
    poll_until(
        "chain index to settle at the chain tip",
        Duration::from_secs(10),
        Duration::from_millis(50),
        || async {
            let tip = index_reader.snapshot_nonfinalized_state().best_tip();
            (u32::from(tip.height) == target_tip).then_some(())
        },
    )
    .await;

    let handle = indexer
        .sync_loop_handle
        .take()
        .expect("sync loop handle present after construction");

    indexer
        .shutdown()
        .await
        .expect("shutdown completes without error");

    let join_outcome = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("sync loop did not exit within 5 s of shutdown");
    let sync_result = join_outcome.expect("sync loop task panicked");
    assert!(
        sync_result.is_ok(),
        "sync loop returned Err on clean shutdown: {sync_result:?}",
    );
}
