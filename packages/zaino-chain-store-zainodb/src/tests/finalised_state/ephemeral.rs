//! Tests for ephemeral (stateless passthrough) finalised state.
//!
//! In ephemeral mode no persistent database is opened; the `FinalisedState`
//! backing is `FinalisedSource::Ephemeral`, which serves finalised reads
//! directly from the `BlockchainSource` (here a `FakeValidator`). These tests
//! assert the passthrough semantics: reads match the source, `db_height` is
//! pinned at `0`, and sync/write paths are no-ops.
//!
//! Attribute note: ephemeral spawn starts only a lightweight status-poll task
//! (no DB validation loop), so each test only needs `.await` (current-thread
//! `#[tokio::test]`); none justifies `multi_thread`.

use std::sync::Arc;

use tempfile::TempDir;
use zaino_common::network::ActivationHeights;
use zaino_proto::proto::utils::{prune_compact_block, PoolTypeFilter};

use crate::config::ZainoDbConfig;
use crate::error::StoreError;
use crate::store::router::FinalisedStateMode;
use crate::store::FinalisedState;
use crate::tests::fixtures::FakeValidator;
use crate::tests::fixtures::{fake_validator_from_vectors, indexed_block_chain, load_test_vectors};
use crate::tests::init_tracing;
use crate::types::Height;
use zaino_chain_store::ChainStoreConfig;
use zaino_status::StatusType;

/// Spawns a `FinalisedState` in ephemeral mode over `source`. The database path
/// is a throwaway tempdir that is never opened (ephemeral mode opens no DB).
pub(crate) async fn spawn_ephemeral_finalised_state(
    source: std::sync::Arc<FakeValidator>,
) -> Result<(TempDir, FinalisedState<FakeValidator>), StoreError> {
    let temp_dir: TempDir = tempfile::tempdir().unwrap();

    // No path at all, which *is* the ephemeral configuration: the tempdir is
    // kept only so the caller can assert nothing was written to it.
    let finalised_state = FinalisedState::spawn(
        ChainStoreConfig::default(),
        ZainoDbConfig::new(ActivationHeights::default().to_regtest_network()),
        source,
    )
    .await?;

    Ok((temp_dir, finalised_state))
}

#[tokio::test]
async fn spawn_is_ephemeral_and_ready() {
    init_tracing();

    let source = fake_validator_from_vectors(&load_test_vectors().unwrap().blocks);
    let (_db_dir, finalised_state) = spawn_ephemeral_finalised_state(source).await.unwrap();

    assert!(
        finalised_state.router().primary_is_ephemeral(),
        "ephemeral mode must route the primary to the ephemeral backing source"
    );

    // Ephemeral status starts `Spawning` and a background poll task flips it to
    // `Ready` once the source answers; wait for that before asserting.
    finalised_state.wait_until_ready().await;
    assert_eq!(finalised_state.status(), StatusType::Ready);
}

#[tokio::test]
async fn db_height_reports_zero() {
    init_tracing();

    // The source holds a full test-vector chain, but an ephemeral finalised
    // state persists nothing, so it reports height 0.
    let source = fake_validator_from_vectors(&load_test_vectors().unwrap().blocks);
    let (_db_dir, finalised_state) = spawn_ephemeral_finalised_state(source).await.unwrap();

    assert_eq!(finalised_state.db_height().await.unwrap(), Some(Height(0)));
}

#[tokio::test]
async fn sync_to_height_is_noop() {
    init_tracing();

    let source = fake_validator_from_vectors(&load_test_vectors().unwrap().blocks);
    let (_db_dir, finalised_state) = spawn_ephemeral_finalised_state(source.clone())
        .await
        .unwrap();

    finalised_state
        .sync_to_height(Height(200), &source)
        .await
        .unwrap();
    finalised_state.wait_until_synced().await;

    assert_eq!(finalised_state.db_height().await.unwrap(), Some(Height(0)));
}

#[tokio::test]
async fn writes_are_noops() {
    init_tracing();

    let blocks = load_test_vectors().unwrap().blocks;
    let source = fake_validator_from_vectors(&blocks.clone());
    let (_db_dir, finalised_state) = spawn_ephemeral_finalised_state(source).await.unwrap();

    // A write against an ephemeral backing is accepted but persists nothing.
    let first_block = indexed_block_chain(&blocks).next().unwrap();
    finalised_state.write_block(first_block).await.unwrap();
    finalised_state
        .delete_block_at_height(Height(1))
        .await
        .unwrap();

    assert_eq!(finalised_state.db_height().await.unwrap(), Some(Height(0)));
}

#[tokio::test]
async fn reader_compact_blocks_match_source() {
    init_tracing();

    let blocks = load_test_vectors().unwrap().blocks;
    let source = fake_validator_from_vectors(&blocks.clone());
    let (_db_dir, finalised_state) = spawn_ephemeral_finalised_state(source).await.unwrap();
    let finalised_state = Arc::new(finalised_state);
    let reader = finalised_state.to_reader();

    for chain_block in indexed_block_chain(&blocks) {
        let height = chain_block.context.index.height;
        let compact_block = chain_block.to_compact_block();

        let reader_default = reader
            .get_compact_block(height, PoolTypeFilter::default())
            .await
            .unwrap();
        let expected_default =
            prune_compact_block(compact_block.clone(), &PoolTypeFilter::default());
        assert_eq!(expected_default, reader_default);

        let reader_all = reader
            .get_compact_block(height, PoolTypeFilter::includes_all())
            .await
            .unwrap();
        let expected_all = prune_compact_block(compact_block, &PoolTypeFilter::includes_all());
        assert_eq!(expected_all, reader_all);
    }
}

#[tokio::test]
async fn reader_compact_block_stream_matches_source() {
    use futures::StreamExt;

    init_tracing();

    let blocks = load_test_vectors().unwrap().blocks;
    let source = fake_validator_from_vectors(&blocks.clone());
    let (_db_dir, finalised_state) = spawn_ephemeral_finalised_state(source).await.unwrap();
    let finalised_state = Arc::new(finalised_state);
    let reader = finalised_state.to_reader();

    let start_height = Height(blocks.first().unwrap().height);
    let end_height = Height(blocks.last().unwrap().height);

    let stream = reader
        .get_compact_block_stream(start_height, end_height, PoolTypeFilter::includes_all())
        .await
        .unwrap();
    futures::pin_mut!(stream);

    let mut expected_next: u32 = start_height.0;
    let mut count: usize = 0;
    while let Some(block_result) = stream.next().await {
        let streamed = block_result.unwrap();
        let streamed_height = u32::try_from(streamed.height).unwrap();
        assert_eq!(streamed_height, expected_next);

        let singular = reader
            .get_compact_block(Height(streamed_height), PoolTypeFilter::includes_all())
            .await
            .unwrap();
        assert_eq!(singular, streamed);

        expected_next = expected_next.saturating_add(1);
        count = count.saturating_add(1);
    }

    let expected_count = (end_height
        .0
        .saturating_sub(start_height.0)
        .saturating_add(1)) as usize;
    assert_eq!(count, expected_count);
}

#[tokio::test]
async fn reader_chain_block_and_header_identity_matches_source() {
    init_tracing();

    let blocks = load_test_vectors().unwrap().blocks;
    let source = fake_validator_from_vectors(&blocks.clone());
    let (_db_dir, finalised_state) = spawn_ephemeral_finalised_state(source).await.unwrap();
    let finalised_state = Arc::new(finalised_state);
    let reader = finalised_state.to_reader();

    // Ephemeral blocks are rebuilt from the source with chainwork 0, so assert
    // identity (height + hash) rather than full `IndexedBlock` equality.
    for chain_block in indexed_block_chain(&blocks) {
        let height = chain_block.context.index.height;
        let hash = *chain_block.hash();

        let block = reader
            .get_chain_block_by_height(height)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(block.height(), height);
        assert_eq!(*block.hash(), hash);

        let header = reader.get_block_header(height).await.unwrap();
        assert_eq!(header.context.index.height, height);
    }
}

#[tokio::test]
async fn shutdown_returns_promptly() {
    super::assert_shutdown_returns_promptly("Ephemeral", spawn_ephemeral_finalised_state).await;
}

/// `ephemeral_finalised_state = true` must be reported as a configuration choice, not as a
/// transient sync state — the two are indistinguishable from `StatusType` alone, which is the
/// ambiguity `FinalisedStateMode` exists to remove.
#[tokio::test]
async fn configured_ephemeral_reports_its_mode() {
    init_tracing();

    let source = fake_validator_from_vectors(&load_test_vectors().unwrap().blocks);
    let (_db_dir, finalised_state) = spawn_ephemeral_finalised_state(source).await.unwrap();

    finalised_state.wait_until_ready().await;

    // `Ready` alone cannot distinguish this from a fully synced persistent database.
    assert_eq!(finalised_state.status(), StatusType::Ready);
    assert_eq!(
        finalised_state.finalised_state_mode(),
        FinalisedStateMode::EphemeralConfigured
    );
}

/// Installing and releasing the ephemeral passthrough must move `finalised_state_mode` between
/// `Persistent` and `EphemeralRouted`, and the released passthrough must hand reads back to the
/// persistent database.
///
/// This is the transition the new routing logs describe, and the one `StatusType` cannot express:
/// the passthrough reports `Ready` for the whole span, so a caller gating on `Ready` alone would
/// see no change across either edge.
///
/// Drives the router directly rather than through `sync_to_height`: that path installs the
/// passthrough in the foreground but releases it from a spawned task, so observing the
/// intermediate state would race the background sync.
///
/// `multi_thread` required: the persistent v1 backend's validation path calls
/// `block_in_place`, which panics on a current-thread runtime.
#[tokio::test(flavor = "multi_thread")]
async fn ephemeral_routing_transitions_are_visible_in_mode() {
    init_tracing();

    let source = fake_validator_from_vectors(&load_test_vectors().unwrap().blocks);
    let (_db_dir, finalised_state) =
        crate::tests::finalised_state::v1::spawn_v1_zaino_db(source.clone())
            .await
            .unwrap();

    // A persistent database serves its own reads before any ephemeral routing is installed.
    assert_eq!(
        finalised_state.finalised_state_mode(),
        FinalisedStateMode::Persistent
    );

    let router = finalised_state.router_arc();
    let network = ActivationHeights::default().to_regtest_network();

    let ephemeral_reference = router
        .init_or_take_ephemeral(
            source.clone(),
            network,
            crate::store::router::EphemeralMode::ReadOnly,
            None,
        )
        .await
        .unwrap();

    assert_eq!(
        finalised_state.finalised_state_mode(),
        FinalisedStateMode::EphemeralRouted,
        "an installed passthrough must be reported as ephemeral, not persistent"
    );

    // Dropping the last reference restores primary routing.
    drop(ephemeral_reference);

    assert_eq!(
        finalised_state.finalised_state_mode(),
        FinalisedStateMode::Persistent,
        "releasing the last ephemeral reference must hand reads back to the persistent database"
    );
}
