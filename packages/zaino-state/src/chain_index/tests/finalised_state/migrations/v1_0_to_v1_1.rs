//! V1.0 to V1.1 migration tests.

use std::path::PathBuf;
use tempfile::TempDir;
use zaino_common::network::ActivationHeights;
use zaino_common::{DatabaseConfig, Network, StorageConfig};

use crate::chain_index::finalised_state::capability::{
    DbCore as _, DbRead as _, DbVersion, MigrationStatus,
};
use crate::chain_index::finalised_state::FinalisedState;
use crate::chain_index::tests::init_tracing;
use crate::chain_index::tests::vectors::{
    build_active_mockchain_source, load_test_vectors, TestVectorData,
};
use crate::{ChainIndexConfig, Height};

#[tokio::test(flavor = "multi_thread")]
async fn v1_0_to_v1_1_metadata_migration() {
    init_tracing();

    let TestVectorData { blocks, .. } = load_test_vectors().unwrap();

    let temp_dir: TempDir = tempfile::tempdir().unwrap();
    let db_path: PathBuf = temp_dir.path().to_path_buf();

    let v1_config = ChainIndexConfig {
        storage: StorageConfig {
            database: DatabaseConfig {
                path: db_path,
                ..Default::default()
            },
            ..Default::default()
        },
        ephemeral: false,
        db_version: 1,
        network: Network::Regtest(ActivationHeights::default()),
    };

    let source = build_active_mockchain_source(150, blocks.clone());

    let zaino_db = FinalisedState::build_db_to_version(
        v1_config,
        source,
        DbVersion {
            major: 1,
            minor: 1,
            patch: 0,
        },
    )
    .await
    .unwrap();

    zaino_db.wait_until_ready().await;

    let metadata = zaino_db.get_metadata().await.unwrap();

    assert_eq!(
        metadata.version,
        DbVersion {
            major: 1,
            minor: 1,
            patch: 0,
        }
    );
    assert_eq!(metadata.migration_status, MigrationStatus::Empty);
    assert_eq!(
        metadata.schema_hash,
        crate::chain_index::finalised_state::finalised_source::v1::DB_SCHEMA_V1_HASH
    );

    let db_height = zaino_db.db_height().await.unwrap().unwrap();
    assert_eq!(db_height, Height(150));

    zaino_db.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn v1_0_to_v1_1_mixed_blockheaderdata_formats() {
    init_tracing();

    let TestVectorData { blocks, .. } = load_test_vectors().unwrap();

    let initial_active_height = Height(150);

    let temp_dir: TempDir = tempfile::tempdir().unwrap();
    let db_path: PathBuf = temp_dir.path().to_path_buf();

    let v1_config = ChainIndexConfig {
        storage: StorageConfig {
            database: DatabaseConfig {
                path: db_path,
                ..Default::default()
            },
            ..Default::default()
        },
        ephemeral: false,
        db_version: 1,
        network: Network::Regtest(ActivationHeights::default()),
    };

    let source = build_active_mockchain_source(initial_active_height.0, blocks.clone());

    let old_db = FinalisedState::build_clean_v1_0_0(&v1_config, source.clone())
        .await
        .unwrap();

    let old_db_height = old_db.db_height().await.unwrap().unwrap();
    assert_eq!(old_db_height, initial_active_height);

    let old_metadata = old_db.get_metadata().await.unwrap();
    assert_eq!(
        old_metadata.version,
        DbVersion {
            major: 1,
            minor: 0,
            patch: 0,
        }
    );
    assert_eq!(old_metadata.migration_status, MigrationStatus::Empty);

    old_db.shutdown().await.unwrap();
    drop(old_db);

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let blocks_to_mine = source.max_chain_height() - source.active_height();
    assert!(
        blocks_to_mine > 0,
        "test vectors must contain blocks above the initial active height"
    );

    source.mine_blocks(blocks_to_mine);

    let target_height = Height(source.active_height());
    assert!(
        target_height > initial_active_height,
        "mock chain source must advance beyond the old v1.0.0 database height"
    );

    let zaino_db = std::sync::Arc::new(
        FinalisedState::spawn(v1_config, source.clone())
            .await
            .unwrap(),
    );

    zaino_db.wait_until_synced().await;

    let migrated_db_height = zaino_db.db_height().await.unwrap().unwrap();
    assert_eq!(
        migrated_db_height, initial_active_height,
        "metadata migration must not append newly active blocks"
    );

    zaino_db
        .sync_to_height(target_height, &source)
        .await
        .unwrap();

    zaino_db.wait_until_synced().await;

    let synced_db_height = zaino_db.db_height().await.unwrap().unwrap();
    assert_eq!(synced_db_height, target_height);

    {
        let reader = zaino_db.to_reader();

        for height in 0..=initial_active_height.0 {
            let height = Height(height);

            let header = reader
                .get_block_header(height)
                .await
                .unwrap_or_else(|error| {
                    panic!(
                        "failed to read v1.0.0-format BlockHeaderData at height {}: {error}",
                        height.0
                    )
                });

            assert_eq!(
                header.context.index.height, height,
                "v1.0.0-format BlockHeaderData at height {} returned wrong height",
                height.0
            );
        }

        for height in (initial_active_height.0 + 1)..=target_height.0 {
            let height = Height(height);

            let header = reader
                .get_block_header(height)
                .await
                .unwrap_or_else(|error| {
                    panic!(
                        "failed to read current-format BlockHeaderData at height {}: {error}",
                        height.0
                    )
                });

            assert_eq!(
                header.context.index.height, height,
                "current-format BlockHeaderData at height {} returned wrong height",
                height.0
            );
        }
    }

    zaino_db.shutdown().await.unwrap();
}
