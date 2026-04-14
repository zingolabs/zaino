//! Holds database migration tests.

use std::path::PathBuf;
use tempfile::TempDir;
use zaino_common::network::ActivationHeights;
use zaino_common::{DatabaseConfig, Network, StorageConfig};

use crate::chain_index::finalised_state::capability::{
    DbCore as _, DbVersion, DbWrite as _, MigrationStatus,
};
use crate::chain_index::finalised_state::db::v1::DB_SCHEMA_V1_HASH;
use crate::chain_index::finalised_state::db::DbBackend;
use crate::chain_index::finalised_state::ZainoDB;
use crate::chain_index::tests::init_tracing;
use crate::chain_index::tests::vectors::{
    build_mockchain_source, load_test_vectors, TestVectorData,
};
use crate::BlockCacheConfig;

#[tokio::test(flavor = "multi_thread")]
async fn v0_to_v1_full() {
    init_tracing();

    let TestVectorData { blocks, .. } = load_test_vectors().unwrap();

    let temp_dir: TempDir = tempfile::tempdir().unwrap();
    let db_path: PathBuf = temp_dir.path().to_path_buf();

    let v0_config = BlockCacheConfig {
        storage: StorageConfig {
            database: DatabaseConfig {
                path: db_path.clone(),
                ..Default::default()
            },
            ..Default::default()
        },
        db_version: 0,
        network: Network::Regtest(ActivationHeights::default()),
    };
    let v1_config = BlockCacheConfig {
        storage: StorageConfig {
            database: DatabaseConfig {
                path: db_path,
                ..Default::default()
            },
            ..Default::default()
        },
        db_version: 1,
        network: Network::Regtest(ActivationHeights::default()),
    };

    let source = build_mockchain_source(blocks.clone());

    // Build v0 database.
    let zaino_db = ZainoDB::spawn(v0_config, source.clone()).await.unwrap();
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
    let zaino_db_2 = ZainoDB::spawn(v1_config, source).await.unwrap();
    zaino_db_2.wait_until_ready().await;
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

    let v0_config = BlockCacheConfig {
        storage: StorageConfig {
            database: DatabaseConfig {
                path: db_path.clone(),
                ..Default::default()
            },
            ..Default::default()
        },
        db_version: 0,
        network: Network::Regtest(ActivationHeights::default()),
    };
    let v1_config = BlockCacheConfig {
        storage: StorageConfig {
            database: DatabaseConfig {
                path: db_path,
                ..Default::default()
            },
            ..Default::default()
        },
        db_version: 1,
        network: Network::Regtest(ActivationHeights::default()),
    };

    let source = build_mockchain_source(blocks.clone());

    // Build v0 database.
    let zaino_db = ZainoDB::spawn(v0_config, source.clone()).await.unwrap();
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
    let zaino_db = DbBackend::spawn_v1(&v1_config).await.unwrap();
    crate::chain_index::tests::vectors::sync_db_with_blockdata(&zaino_db, blocks.clone(), Some(50))
        .await;

    dbg!(zaino_db.shutdown().await.unwrap());

    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

    // Open v1 database and check migration.
    let zaino_db_2 = ZainoDB::spawn(v1_config, source).await.unwrap();
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

    let v0_config = BlockCacheConfig {
        storage: StorageConfig {
            database: DatabaseConfig {
                path: db_path.clone(),
                ..Default::default()
            },
            ..Default::default()
        },
        db_version: 0,
        network: Network::Regtest(ActivationHeights::default()),
    };
    let v1_config = BlockCacheConfig {
        storage: StorageConfig {
            database: DatabaseConfig {
                path: db_path,
                ..Default::default()
            },
            ..Default::default()
        },
        db_version: 1,
        network: Network::Regtest(ActivationHeights::default()),
    };

    let source = build_mockchain_source(blocks.clone());

    // Build v0 database.
    let zaino_db = ZainoDB::spawn(v0_config, source.clone()).await.unwrap();
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
    let zaino_db = DbBackend::spawn_v1(&v1_config).await.unwrap();
    crate::chain_index::tests::vectors::sync_db_with_blockdata(&zaino_db, blocks.clone(), None)
        .await;

    dbg!(zaino_db.shutdown().await.unwrap());

    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

    // Open v1 database and check migration.
    let zaino_db_2 = ZainoDB::spawn(v1_config, source).await.unwrap();
    zaino_db_2.wait_until_ready().await;
    dbg!(zaino_db_2.status());
    let db_height = dbg!(zaino_db_2.db_height().await.unwrap()).unwrap();
    assert_eq!(db_height.0, 200);
    dbg!(zaino_db_2.shutdown().await.unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn v1_0_to_v1_1_metadata_migration() {
    init_tracing();

    // Prepare test blocks/source (we won't rely on heavy rebuild in this metadata-only migration)
    let TestVectorData { blocks, .. } = load_test_vectors().unwrap();

    let temp_dir: TempDir = tempfile::tempdir().unwrap();
    let db_path: PathBuf = temp_dir.path().to_path_buf();

    // BlockCacheConfig: use v1 target like other tests
    let v1_config = BlockCacheConfig {
        storage: StorageConfig {
            database: DatabaseConfig {
                path: db_path.clone(),
                ..Default::default()
            },
            ..Default::default()
        },
        db_version: 1,
        network: Network::Regtest(ActivationHeights::default()),
    };

    let source = build_mockchain_source(blocks.clone());

    // Build v1 database.
    let zaino_db = ZainoDB::spawn(v1_config.clone(), source.clone())
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

    // 2) Coerce the metadata to look like an older 1.0.0 DB with a stale schema hash and an
    //    in-progress migration status so the migration has work to do.
    let mut metadata = zaino_db.get_metadata().await.unwrap();
    metadata.version = DbVersion {
        major: 1,
        minor: 0,
        patch: 0,
    };
    // An obviously different schema hash (any non-matching 32 bytes will do)
    metadata.schema_hash = [0u8; 32];
    // Set a non-empty migration status to ensure migration clears it
    metadata.migration_status = MigrationStatus::PartialBuidInProgress;

    zaino_db.router().update_metadata(metadata).await.unwrap();

    // shutdown this backend so ZainoDB::spawn will open the same DB and perform migration
    dbg!(zaino_db.shutdown().await.unwrap());

    // Let the filesystem settle (tests elsewhere do a brief sleep)
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // 3) Spawn ZainoDB which should detect current_version == 1.0.0 < target 1.1.0 and run the metadata
    //    migration. We await until ready so migration completes.
    let zaino_db = ZainoDB::spawn(v1_config, source).await.unwrap();
    zaino_db.wait_until_ready().await;

    // 4) Read persisted metadata and assert migration effects.
    let post_meta = zaino_db.get_metadata().await.unwrap();
    assert_eq!(
        post_meta.version,
        DbVersion {
            major: 1,
            minor: 1,
            patch: 0
        }
    );
    assert_eq!(post_meta.migration_status, MigrationStatus::Empty);
    assert_eq!(post_meta.schema_hash, DB_SCHEMA_V1_HASH);

    zaino_db.shutdown().await.unwrap();
}
