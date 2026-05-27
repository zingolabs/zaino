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
//!
//! The one exception is the trailing **red driver** for #1096
//! (`best_chaintip_derives_tip_from_nfs_snapshot_not_validator_passthrough`).
//! Unlike the characterization tests above — which pin behavior that must
//! survive the refactor unchanged — that test is *failing on purpose* and is
//! expected to be rewritten when the still-syncing variant is removed. It
//! pins cold-start shape precisely because it is driving that variant's
//! elimination, so the churn it incurs is the point, not an accident.

use super::{load_test_vectors_and_sync_chain_index, poll::poll_until, MockchainMode};
use crate::chain_index::non_finalised_state::ChainIndexSnapshot;
use crate::chain_index::{finalized_height_floor, ChainIndex};
use std::time::Duration;
use tokio::time::sleep;

/// **B**: After the chain index has finished its first sync iteration,
/// the lowest-height block in the NFS snapshot is the same block the
/// finalized DB has at its tip. The two layers must overlap exactly at
/// the seam (`finalized_db.db_height()`).
#[tokio::test(flavor = "multi_thread")]
async fn nfs_lowest_block_matches_finalized_db_tip() {
    let (_blocks, _indexer, index_reader, mockchain) =
        load_test_vectors_and_sync_chain_index(MockchainMode::Active).await;

    let snapshot = index_reader.snapshot_nonfinalized_state().await.unwrap();
    let nfs = snapshot
        .get_nfs_snapshot()
        .expect("NFS exists after harness completes finalized sync");

    let seam_height = finalized_height_floor(mockchain.active_height());
    let nfs_seam_hash = nfs
        .heights_to_hashes
        .get(&seam_height)
        .copied()
        .expect("NFS retains the block at the finalized-DB tip height");

    let finalized_db_tip_block = index_reader
        .finalized_state
        .get_chain_block_by_height(seam_height)
        .await
        .expect("read finalized DB")
        .expect("finalized DB has a block at its tip height");

    assert_eq!(
        nfs_seam_hash,
        *finalized_db_tip_block.hash(),
        "block at seam height {} must match between NFS and finalized DB",
        seam_height.0,
    );
}

/// **D**: A block in the NFS is evicted once the finalized DB advances
/// past its height. Pins the trim step inside `update`
/// (`non_finalised_state.rs:remove_finalized_blocks`, which retains
/// only blocks with `height >= finalized_height`).
#[tokio::test(flavor = "multi_thread")]
async fn block_is_evicted_from_nfs_when_finalized_advances_past_it() {
    let (_blocks, _indexer, index_reader, mockchain) =
        load_test_vectors_and_sync_chain_index(MockchainMode::Active).await;

    let initial_seam_height = finalized_height_floor(mockchain.active_height());

    let initial_snapshot = index_reader.snapshot_nonfinalized_state().await.unwrap();
    let initial_nfs = initial_snapshot
        .get_nfs_snapshot()
        .expect("NFS exists after harness");
    let target_hash = *initial_nfs
        .heights_to_hashes
        .get(&initial_seam_height)
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
            .contains_key(&initial_seam_height),
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
        load_test_vectors_and_sync_chain_index(MockchainMode::Active).await;

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
        load_test_vectors_and_sync_chain_index(MockchainMode::Active).await;

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
/// pokes at the same property — *blocks at the iter's pre-mine finalized
/// height should be evicted from the NFS once the source advances past them*
/// — but does so by calling `mine_blocks` from the test thread and racing
/// the sync worker for the iter-start window. Whether the race fires depends
/// on scheduler quirks; in CI it can pass while the bug is fully present.
///
/// This test forces the race window using the one-shot
/// [`MockchainSource::arm_one_shot_get_block_hook`]. The hook fires the
/// *first* time the worker requests `get_block(Height(_))`, which is the
/// first call inside iter N's NFS-sync while loop *after* iter N has already
/// committed to `chain_height = initial_active` and called
/// `fs.sync_to_height(finalized_height_floor(initial_active))` as a no-op.
/// From inside the hook the test mines 20 blocks; the same `get_block` call
/// then reads the *new* `active_chain_height = initial_active + 20` and
/// returns block `initial_active + 1`, which the worker's loop happily
/// extends past the iter's commitment all the way to `initial_active + 20`.
/// The iter's `update` step uses `finalized_height_floor(initial_active)`
/// for the trim and publishes a snapshot whose lowest height is *below* the
/// post-mine seam.
///
/// **The assertion below should pass once the race is fixed and fail every
/// run while it is present.** While present, the published NFS contains
/// blocks down to the pre-mine finalized height (the seam block from before
/// the iter began), so `target_hash` — the block at that pre-mine finalized
/// height — is still in `blocks`. After the fix, the iter would cap its NFS
/// extension at the committed `chain_height`, so the post-mine blocks would
/// land in iter N+1 (which would compute the correct post-mine finalized
/// height and trim properly).
#[tokio::test(flavor = "multi_thread")]
async fn race_pre_mine_finalized_height_block_is_evicted_when_source_advances_mid_iter() {
    let (_blocks, _indexer, index_reader, mockchain) =
        load_test_vectors_and_sync_chain_index(MockchainMode::Active).await;

    let initial_active = mockchain.active_height();
    let pre_mine_finalized_height = finalized_height_floor(initial_active);

    let initial_snapshot = index_reader.snapshot_nonfinalized_state().await.unwrap();
    let initial_nfs = initial_snapshot
        .get_nfs_snapshot()
        .expect("NFS exists after harness");
    let target_hash = *initial_nfs
        .heights_to_hashes
        .get(&pre_mine_finalized_height)
        .expect("NFS retains the block at the finalized-DB tip height");
    assert!(
        initial_nfs.blocks.contains_key(&target_hash),
        "precondition: block at pre-mine finalized height is in NFS",
    );

    // Arm the race window: when the worker's NFS-sync while loop makes its
    // first `get_block(Height(_))` call, advance the chain by 20 from inside
    // the hook. The advance happens before the source's `valid_height` check,
    // so the same call returns a block at the new height and the worker's
    // loop extends past its iter-committed `chain_height` — exactly the
    // production scenario where the validator produces blocks while zaino is
    // mid-iteration.
    let advance: u32 = 20;
    let mc = mockchain.clone();
    mockchain.arm_one_shot_get_block_hook(Box::new(move || mc.mine_blocks(advance)));

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
        "block at pre-mine finalized height (height {}) must be evicted after the \
         source advances mid-iter; published NFS overshoots its iter-committed \
         seam (#1126)",
        pre_mine_finalized_height.0,
    );
}

/// **Red driver for #1096** (NOT a surviving characterization test — see the
/// module-level doc; this one is *failing on purpose* and is expected to be
/// rewritten when the still-syncing variant is removed).
///
/// Target invariant: `best_chaintip` must derive the chain tip from the
/// non-finalized snapshot in *every* availability state — it must never fall
/// back to a validator passthrough.
///
/// Today the lazy design hands out
/// [`ChainIndexSnapshot::StillSyncingFinalizedState`] during the cold-start
/// window, before the NFS slot is populated. In that variant `best_chaintip`
/// (`chain_index.rs`, the `StillSyncingFinalizedState` arm) has no snapshot
/// tip to read, so it round-trips to the validator and reports the *finalized
/// floor* (`validator_finalized_height`) as the tip — a stale height, and a
/// fallible call that surfaces `database_hole` if the validator can't serve
/// the floor block.
///
/// After #1096 there is no still-syncing variant: the snapshot always carries
/// an NFS `best_tip`, so `best_chaintip` reads it directly and reports the
/// real tip with no validator call. The test will then be rewritten to assert
/// the invariant over a snapshot returned by `snapshot_nonfinalized_state()`.
///
/// multi_thread: depends on the harness's background sync loop advancing the
/// NFS concurrently with the setup's poll-until-ready loop.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "red driver for #1096: fails on purpose against today's still-syncing \
            variant, which passes through to the validator and reports the \
            finalized floor instead of the tip. Pins the target invariant to \
            drive that variant's elimination; rewritten against \
            snapshot_nonfinalized_state() once #1096 lands."]
async fn best_chaintip_derives_tip_from_nfs_snapshot_not_validator_passthrough() {
    let (_blocks, _indexer, index_reader, mockchain) =
        load_test_vectors_and_sync_chain_index(MockchainMode::Active).await;

    // The chain tip the always-present NFS snapshot reports: the harness syncs
    // the NFS to exactly the source's active height before returning.
    let chain_tip = mockchain.active_height();

    // The cold-start variant the lazy design can hand out instead. Its
    // `validator_finalized_height` is the true floor — exactly what
    // `snapshot_nonfinalized_state()` computes while the NFS slot is `None`.
    let cold_start_snapshot = ChainIndexSnapshot::StillSyncingFinalizedState {
        validator_finalized_height: finalized_height_floor(chain_tip),
    };

    let tip = index_reader
        .best_chaintip(&cold_start_snapshot)
        .await
        .expect("best_chaintip resolves against a still-syncing snapshot");

    assert_eq!(
        tip.height.0,
        chain_tip,
        "best_chaintip must derive the tip from the NFS snapshot and report the \
         chain tip ({chain_tip}) in every availability state; today the cold-start \
         variant passes through to the validator and reports the finalized floor \
         ({}) instead (#1096)",
        finalized_height_floor(chain_tip).0,
    );
}
