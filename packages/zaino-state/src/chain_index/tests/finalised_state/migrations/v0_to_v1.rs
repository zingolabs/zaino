//! V0 to V1 migration tests.

use std::path::PathBuf;
use tempfile::TempDir;
use zaino_common::network::ActivationHeights;
use zaino_common::{DatabaseConfig, Network, StorageConfig};

use crate::chain_index::finalised_state::capability::DbCore as _;
use crate::chain_index::finalised_state::finalised_source::FinalisedSource;
use crate::chain_index::finalised_state::FinalisedState;
use crate::chain_index::source::mockchain_source::MockchainSource;
use crate::chain_index::tests::init_tracing;
use crate::chain_index::tests::vectors::{
    build_mockchain_source, load_test_vectors, TestVectorData,
};
use crate::ChainIndexConfig;

#[tokio::test(flavor = "multi_thread")]
async fn v0_to_v1_full() {
    init_tracing();

    let TestVectorData { blocks, .. } = load_test_vectors().unwrap();

    let temp_dir: TempDir = tempfile::tempdir().unwrap();
    let db_path: PathBuf = temp_dir.path().to_path_buf();

    let v0_config = ChainIndexConfig {
        storage: StorageConfig {
            database: DatabaseConfig {
                path: db_path.clone(),
                ..Default::default()
            },
            ..Default::default()
        },
        ephemeral: false,
        db_version: 0,
        network: Network::Regtest(ActivationHeights::default()),
    };
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

    let source = build_mockchain_source(blocks.clone());

    // Build v0 database.
    let zaino_db = FinalisedState::spawn(v0_config, source.clone())
        .await
        .unwrap();
    crate::chain_index::tests::vectors::sync_db_with_blockdata(
        zaino_db.router(),
        blocks.clone(),
        None,
    )
    .await;

    zaino_db.wait_until_ready().await;
    dbg!(zaino_db.status());
    dbg!(zaino_db.db_height().await.unwrap());
    dbg!(zaino_db.shutdown().await.unwrap());

    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

    // Open v1 database and check migration.
    let zaino_db_2 = FinalisedState::spawn(v1_config, source).await.unwrap();
    zaino_db_2.wait_until_synced().await;
    dbg!(zaino_db_2.status());
    let db_height = dbg!(zaino_db_2.db_height().await.unwrap()).unwrap();
    assert_eq!(db_height.0, 200);
    dbg!(zaino_db_2.shutdown().await.unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn v0_to_v1_interrupted() {
    init_tracing();

    let blocks = load_test_vectors().unwrap().blocks;

    let temp_dir: TempDir = tempfile::tempdir().unwrap();
    let db_path: PathBuf = temp_dir.path().to_path_buf();

    let v0_config = ChainIndexConfig {
        storage: StorageConfig {
            database: DatabaseConfig {
                path: db_path.clone(),
                ..Default::default()
            },
            ..Default::default()
        },
        ephemeral: false,
        db_version: 0,
        network: Network::Regtest(ActivationHeights::default()),
    };
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

    let source = build_mockchain_source(blocks.clone());

    // Build v0 database.
    let zaino_db = FinalisedState::spawn(v0_config, source.clone())
        .await
        .unwrap();
    crate::chain_index::tests::vectors::sync_db_with_blockdata(
        zaino_db.router(),
        blocks.clone(),
        None,
    )
    .await;
    zaino_db.wait_until_ready().await;
    dbg!(zaino_db.status());
    dbg!(zaino_db.db_height().await.unwrap());
    dbg!(zaino_db.shutdown().await.unwrap());

    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

    // Partial build v1 database.
    let zaino_db: FinalisedSource<MockchainSource> =
        FinalisedSource::spawn_v1(&v1_config).await.unwrap();
    crate::chain_index::tests::vectors::sync_db_with_blockdata(&zaino_db, blocks.clone(), Some(50))
        .await;

    dbg!(zaino_db.shutdown().await.unwrap());

    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

    // Open v1 database and check migration.
    let zaino_db_2 = FinalisedState::spawn(v1_config, source).await.unwrap();
    zaino_db_2.wait_until_ready().await;
    dbg!(zaino_db_2.status());
    let db_height = dbg!(zaino_db_2.db_height().await.unwrap()).unwrap();
    assert_eq!(db_height.0, 200);
    dbg!(zaino_db_2.shutdown().await.unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn v0_to_v1_partial() {
    init_tracing();

    let blocks = load_test_vectors().unwrap().blocks;

    let temp_dir: TempDir = tempfile::tempdir().unwrap();
    let db_path: PathBuf = temp_dir.path().to_path_buf();

    let v0_config = ChainIndexConfig {
        storage: StorageConfig {
            database: DatabaseConfig {
                path: db_path.clone(),
                ..Default::default()
            },
            ..Default::default()
        },
        ephemeral: false,
        db_version: 0,
        network: Network::Regtest(ActivationHeights::default()),
    };
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

    let source = build_mockchain_source(blocks.clone());

    // Build v0 database.
    let zaino_db = FinalisedState::spawn(v0_config, source.clone())
        .await
        .unwrap();
    crate::chain_index::tests::vectors::sync_db_with_blockdata(
        zaino_db.router(),
        blocks.clone(),
        None,
    )
    .await;

    zaino_db.wait_until_ready().await;
    dbg!(zaino_db.status());
    dbg!(zaino_db.db_height().await.unwrap());
    dbg!(zaino_db.shutdown().await.unwrap());

    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

    // Partial build v1 database.
    let zaino_db: FinalisedSource<MockchainSource> =
        FinalisedSource::spawn_v1(&v1_config).await.unwrap();
    crate::chain_index::tests::vectors::sync_db_with_blockdata(&zaino_db, blocks.clone(), None)
        .await;

    dbg!(zaino_db.shutdown().await.unwrap());

    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

    // Open v1 database and check migration.
    let zaino_db_2 = FinalisedState::spawn(v1_config, source).await.unwrap();
    zaino_db_2.wait_until_ready().await;
    dbg!(zaino_db_2.status());
    let db_height = dbg!(zaino_db_2.db_height().await.unwrap()).unwrap();
    assert_eq!(db_height.0, 200);
    dbg!(zaino_db_2.shutdown().await.unwrap());
}
