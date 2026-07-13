//! Tests for ephemeral (stateless passthrough) finalised state.
//!
//! In ephemeral mode no persistent database is opened; the `FinalisedState`
//! backing is `FinalisedSource::Ephemeral`, which serves finalised reads
//! directly from the `BlockchainSource` (here a `MockchainSource`). These tests
//! assert the passthrough semantics: reads match the source, `db_height` is
//! pinned at `0`, and sync/write paths are no-ops.
//!
//! Attribute note: ephemeral spawn starts only a lightweight status-poll task
//! (no DB validation loop), so each test only needs `.await` (current-thread
//! `#[tokio::test]`); none justifies `multi_thread`.

use std::path::PathBuf;
use std::sync::Arc;

use tempfile::TempDir;
use zaino_common::network::ActivationHeights;
use zaino_common::{DatabaseConfig, Network, StorageConfig};
use zaino_proto::proto::utils::{compact_block_with_pool_types, PoolTypeFilter};

use crate::chain_index::finalised_state::FinalisedState;
use crate::chain_index::source::mockchain_source::MockchainSource;
use crate::chain_index::tests::init_tracing;
use crate::chain_index::tests::vectors::{
    build_mockchain_source, indexed_block_chain, load_test_vectors,
};
use crate::error::FinalisedStateError;
use crate::{ChainIndexConfig, Height, StatusType};

/// Spawns a `FinalisedState` in ephemeral mode over `source`. The database path
/// is a throwaway tempdir that is never opened (ephemeral mode opens no DB).
pub(crate) async fn spawn_ephemeral_finalised_state(
    source: MockchainSource,
) -> Result<(TempDir, FinalisedState<MockchainSource>), FinalisedStateError> {
    let temp_dir: TempDir = tempfile::tempdir().unwrap();
    let db_path: PathBuf = temp_dir.path().to_path_buf();

    let config = ChainIndexConfig {
        storage: StorageConfig {
            database: DatabaseConfig {
                path: db_path,
                ..Default::default()
            },
            ..Default::default()
        },
        ephemeral: true,
        db_version: 1,
        network: Network::Regtest(ActivationHeights::default()),
    };

    let finalised_state = FinalisedState::spawn(config, source).await?;

    Ok((temp_dir, finalised_state))
}

#[tokio::test]
async fn spawn_is_ephemeral_and_ready() {
    init_tracing();

    let source = build_mockchain_source(load_test_vectors().unwrap().blocks);
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
    let source = build_mockchain_source(load_test_vectors().unwrap().blocks);
    let (_db_dir, finalised_state) = spawn_ephemeral_finalised_state(source).await.unwrap();

    assert_eq!(finalised_state.db_height().await.unwrap(), Some(Height(0)));
}

#[tokio::test]
async fn sync_to_height_is_noop() {
    init_tracing();

    let source = build_mockchain_source(load_test_vectors().unwrap().blocks);
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
    let source = build_mockchain_source(blocks.clone());
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
    let source = build_mockchain_source(blocks.clone());
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
        let expected_default = compact_block_with_pool_types(
            compact_block.clone(),
            &PoolTypeFilter::default().to_pool_types_vector(),
        );
        assert_eq!(expected_default, reader_default);

        let reader_all = reader
            .get_compact_block(height, PoolTypeFilter::includes_all())
            .await
            .unwrap();
        let expected_all = compact_block_with_pool_types(
            compact_block,
            &PoolTypeFilter::includes_all().to_pool_types_vector(),
        );
        assert_eq!(expected_all, reader_all);
    }
}

#[tokio::test]
async fn reader_compact_block_stream_matches_source() {
    use futures::StreamExt;

    init_tracing();

    let blocks = load_test_vectors().unwrap().blocks;
    let source = build_mockchain_source(blocks.clone());
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
    let source = build_mockchain_source(blocks.clone());
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
