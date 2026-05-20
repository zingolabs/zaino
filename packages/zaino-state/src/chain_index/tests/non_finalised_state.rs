//! Regression tests pinning observable lifecycle behavior of the
//! [`NonFinalizedState`](crate::NonFinalizedState) as it stands today,
//! against the refactor tracked in
//! https://github.com/zingolabs/zaino/issues/1096 (collapse the lazy
//! `Arc<ArcSwapOption<NFS>>` slot into an always-present `Arc<NFS>`
//! with per-field Provisional/Resolved availability).
//!
//! Pinned invariants — each must survive the refactor under whatever
//! its new shape becomes:
//!
//! - **B**: lowest-height block in the NFS overlaps the finalized-DB
//!   tip (the seam between the two layers is consistent).
//! - **D**: blocks are evicted from the NFS once the finalized DB
//!   crosses their height.
//! - **F**: once the NFS is published, snapshots never observe its
//!   absence (the slot does not flip back to "still syncing").
//! - **G**: `shutdown()` causes the sync loop to terminate cleanly.
//!
//! Tests of the cold-start "still-syncing" variant are deliberately
//! omitted: that variant is being eliminated, and pinning its shape
//! would create immediate test churn at the refactor PR.

use super::{load_test_vectors_and_sync_chain_index, poll::poll_until};
use crate::chain_index::{anchor_height, ChainIndex};
use std::time::Duration;
use tokio::time::sleep;
use zebra_chain::block::Height as ZebraHeight;

/// **B**: After the chain index has finished its first sync iteration,
/// the lowest-height block in the NFS snapshot is the same block the
/// finalized DB has at its tip. The two layers must overlap exactly at
/// the seam (`finalized_db.db_height()`).
#[tokio::test(flavor = "multi_thread")]
async fn nfs_lowest_block_matches_finalized_db_tip() {
    let (_blocks, _indexer, index_reader, mockchain) =
        load_test_vectors_and_sync_chain_index(true).await;

    let snapshot = index_reader.snapshot_nonfinalized_state().await.unwrap();
    let nfs = snapshot
        .get_nfs_snapshot()
        .expect("NFS exists after harness completes finalized sync");

    let anchor_height = anchor_height(mockchain.active_height());
    let nfs_seam_hash = nfs
        .heights_to_hashes
        .get(&anchor_height)
        .copied()
        .expect("NFS retains the block at the finalized-DB tip height");

    let finalized_db_tip_block = index_reader
        .finalized_state
        .get_chain_block_by_height(anchor_height)
        .await
        .expect("read finalized DB")
        .expect("finalized DB has a block at its tip height");

    assert_eq!(
        nfs_seam_hash,
        *finalized_db_tip_block.hash(),
        "block at seam height {} must match between NFS and finalized DB",
        anchor_height.0,
    );
}

/// **D**: A block in the NFS is evicted once the finalized DB advances
/// past its height. Pins the trim step inside `update`
/// (`non_finalised_state.rs:remove_finalized_blocks`, which retains
/// only blocks with `height >= finalized_height`).
#[tokio::test(flavor = "multi_thread")]
async fn block_is_evicted_from_nfs_when_finalized_advances_past_it() {
    let (_blocks, _indexer, index_reader, mockchain) =
        load_test_vectors_and_sync_chain_index(true).await;

    let initial_anchor_height = anchor_height(mockchain.active_height());

    let initial_snapshot = index_reader.snapshot_nonfinalized_state().await.unwrap();
    let initial_nfs = initial_snapshot
        .get_nfs_snapshot()
        .expect("NFS exists after harness");
    let target_hash = *initial_nfs
        .heights_to_hashes
        .get(&initial_anchor_height)
        .expect("NFS retains the block at the finalized-DB tip height");
    assert!(
        initial_nfs.blocks.contains_key(&target_hash),
        "precondition: block at seam height is in NFS",
    );

    mockchain.mine_blocks(20);
    let post_mine_active_height = mockchain.active_height();

    // Poll the *NFS tip*, not `finalized_state.db_height()`:
    // `fs.sync_to_height` advances the finalized DB BEFORE
    // `nfs.sync().update()` runs the trim, so polling the finalized
    // tip races the snapshot read against `update`'s CAS swap. The
    // NFS reaching the post-mine chain tip is only observable after
    // `update` has published the trimmed snapshot.
    poll_until(
        "NFS tip to catch up to the mined chain (post-trim state)",
        Duration::from_secs(10),
        Duration::from_millis(25),
        || async {
            let snapshot = index_reader.snapshot_nonfinalized_state().await.ok()?;
            let nfs = snapshot.get_nfs_snapshot()?;
            (nfs.best_tip.height.0 == post_mine_active_height).then_some(())
        },
    )
    .await;

    let later_snapshot = index_reader.snapshot_nonfinalized_state().await.unwrap();
    let later_nfs = later_snapshot
        .get_nfs_snapshot()
        .expect("NFS still exists after advance");

    assert!(
        !later_nfs.blocks.contains_key(&target_hash),
        "block at original seam height must have been evicted from NFS",
    );
    assert!(
        !later_nfs
            .heights_to_hashes
            .contains_key(&initial_anchor_height),
        "heights_to_hashes must no longer reference the original seam height",
    );
}

/// **F**: Once the NFS slot is populated, every subsequent snapshot
/// observes the NFS — the slot never reverts to "still syncing." Today
/// this is a property of `Arc<ArcSwapOption<NFS>>` with the sync loop
/// as the sole writer; the refactor must preserve the consumer-visible
/// invariant (snapshots always carry an NFS) under its new shape.
#[tokio::test(flavor = "multi_thread")]
async fn nfs_slot_is_monotonic_post_init() {
    let (_blocks, _indexer, index_reader, _mockchain) =
        load_test_vectors_and_sync_chain_index(true).await;

    for i in 0..10 {
        let snapshot = index_reader.snapshot_nonfinalized_state().await.unwrap();
        assert!(
            snapshot.get_nfs_snapshot().is_some(),
            "iteration {i}: post-init snapshot must contain an NFS",
        );
        sleep(Duration::from_millis(100)).await;
    }
}

/// **G**: `shutdown()` causes the sync loop to observe `Closing` on its
/// next iteration check and return `Ok(())`. Pins cooperative shutdown
/// (no `JoinHandle::abort`, no `Drop` impl).
///
/// Uses default timings (NOT `SyncTimings::fast`): the in-iteration
/// `status.store(Syncing | Ready | RecoverableError)` writes overwrite
/// the `Closing` flag set by `shutdown()`, so the cooperative exit only
/// fires when `shutdown()` lands while the loop is in its post-success
/// `interval` sleep. The 500 ms interval gives that window enough room
/// to dominate steady state; fast timings shrink it to ~50 ms and the
/// loop instead exits ~48 s later via failure-escalation once
/// `finalized_db.shutdown()` makes every subsequent `fs.*` call fail.
/// We additionally poll for the NFS to reach the chain tip — that's
/// only true after iter 1's `update` has CAS-swapped, putting the loop
/// safely into interval sleep.
#[tokio::test(flavor = "multi_thread")]
async fn shutdown_terminates_sync_loop_cleanly() {
    let (_blocks, mut indexer, index_reader, mockchain) =
        load_test_vectors_and_sync_chain_index(true).await;

    let target_tip = mockchain.active_height();
    poll_until(
        "indexer to publish NFS at chain tip (loop settled in interval sleep)",
        Duration::from_secs(10),
        Duration::from_millis(50),
        || async {
            let snapshot = index_reader.snapshot_nonfinalized_state().await.ok()?;
            let nfs = snapshot.get_nfs_snapshot()?;
            (nfs.best_tip.height.0 == target_tip).then_some(())
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

/// Deterministic reproducer for the race tracked in
/// https://github.com/zingolabs/zaino/issues/1126.
///
/// Sibling test `block_is_evicted_from_nfs_when_finalized_advances_past_it`
/// pokes at the same property — *blocks at the iter's pre-mine anchor should
/// be evicted from the NFS once the source advances past them* — but does so
/// by calling `mine_blocks` from the test thread and racing the sync worker
/// for the iter-start window. Whether the race fires depends on scheduler
/// quirks; in CI it can pass while the bug is fully present.
///
/// This test forces the race window using the one-shot
/// [`MockchainSource::arm_one_shot_get_block_hook`]. The hook fires the
/// *first* time the worker requests `get_block(Height(initial_active + 1))`,
/// which is the first call inside iter N's NFS-sync while loop *after* iter N
/// has already committed to `chain_height = initial_active` and called
/// `fs.sync_to_height(anchor_height(initial_active))` as a no-op. From inside
/// the hook the test silently mines 20 blocks (no source-change broadcast, so
/// the worker doesn't see a separate wake-up). The same `get_block` call then
/// reads the *new* `active_chain_height = initial_active + 20` and returns
/// block `initial_active + 1`, which the worker's loop happily extends past
/// the iter's commitment all the way to `initial_active + 20`. The iter's
/// `update` step uses `anchor_height(initial_active) = anchor_height_pre_mine`
/// for the trim and publishes a snapshot whose lowest height is *below* the
/// post-mine seam.
///
/// **The assertion below should pass once the race is fixed and fail every
/// run while it is present.** While present, the published NFS contains
/// blocks down to the pre-mine anchor (the seam block from before the iter
/// began), so `target_hash` — the block at that pre-mine anchor — is still
/// in `blocks`. After the fix, the iter would cap its NFS extension at the
/// committed `chain_height`, so the post-mine blocks would land in iter N+1
/// (which would compute the correct post-mine anchor and trim properly).
#[tokio::test(flavor = "multi_thread")]
async fn race_pre_mine_anchor_block_is_evicted_when_source_advances_mid_iter() {
    let (_blocks, _indexer, index_reader, mockchain) =
        load_test_vectors_and_sync_chain_index(true).await;

    let initial_active = mockchain.active_height();
    let pre_mine_anchor = anchor_height(initial_active);

    let initial_snapshot = index_reader.snapshot_nonfinalized_state().await.unwrap();
    let initial_nfs = initial_snapshot
        .get_nfs_snapshot()
        .expect("NFS exists after harness");
    let target_hash = *initial_nfs
        .heights_to_hashes
        .get(&pre_mine_anchor)
        .expect("NFS retains the block at the finalized-DB tip height");
    assert!(
        initial_nfs.blocks.contains_key(&target_hash),
        "precondition: block at pre-mine anchor is in NFS",
    );

    // Arm the race window: when iter N's NFS-sync while loop calls
    // `get_block(initial_active + 1)`, silently advance the chain by 20.
    // `mine_blocks_silent` does not fire the source-change broadcast, so
    // the worker continues its current iter — exactly the production
    // scenario where the validator produces blocks while zaino is
    // mid-iteration.
    let advance: u32 = 20;
    let mc = mockchain.clone();
    mockchain.arm_one_shot_get_block_hook(
        ZebraHeight(initial_active + 1),
        Box::new(move || mc.mine_blocks_silent(advance)),
    );

    let post_mine_active = initial_active + advance;
    poll_until(
        "NFS tip to reach post-mine height (race window forced)",
        Duration::from_secs(10),
        Duration::from_millis(25),
        || async {
            let snapshot = index_reader.snapshot_nonfinalized_state().await.ok()?;
            let nfs = snapshot.get_nfs_snapshot()?;
            (nfs.best_tip.height.0 == post_mine_active).then_some(())
        },
    )
    .await;

    let later_snapshot = index_reader.snapshot_nonfinalized_state().await.unwrap();
    let later_nfs = later_snapshot
        .get_nfs_snapshot()
        .expect("NFS still exists after advance");

    assert!(
        !later_nfs.blocks.contains_key(&target_hash),
        "block at pre-mine anchor (height {}) must be evicted after the \
         source advances mid-iter; published NFS overshoots its iter-committed \
         seam (#1126)",
        pre_mine_anchor.0,
    );
}
