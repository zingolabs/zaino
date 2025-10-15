//! Holds database migration tests.

use std::path::PathBuf;
use tempfile::TempDir;
use zaino_common::network::ActivationHeights;
use zaino_common::{DatabaseConfig, Network, StorageConfig};

use crate::chain_index::finalised_state::capability::{DbCore as _, DbWrite as _};
use crate::chain_index::finalised_state::db::DbBackend;
use crate::chain_index::finalised_state::ZainoDB;
use crate::chain_index::tests::init_tracing;
use crate::chain_index::tests::vectors::{build_mockchain_source, load_test_vectors};
use crate::{BlockCacheConfig, ChainWork, IndexedBlock};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v0_to_v1_full() {
    init_tracing();

    let (blocks, _faucet, _recipient) = load_test_vectors().unwrap();

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

        no_sync: false,
        no_db: false,
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

        no_sync: false,
        no_db: false,
    };

    let source = build_mockchain_source(blocks.clone());

    // Build v0 database.
    let zaino_db = ZainoDB::spawn(v0_config, source.clone()).await.unwrap();
    let mut parent_chain_work = ChainWork::from_u256(0.into());
    for (
        _h,
        zebra_block,
        (sapling_root, sapling_root_size, orchard_root, orchard_root_size),
        _treestates,
    ) in blocks.clone()
    {
        let chain_block = IndexedBlock::try_from((
            &zebra_block,
            sapling_root,
            sapling_root_size as u32,
            orchard_root,
            orchard_root_size as u32,
            &parent_chain_work,
            &zaino_common::Network::Regtest(ActivationHeights::default()).to_zebra_network(),
        ))
        .unwrap();
        parent_chain_work = *chain_block.chainwork();

        zaino_db.write_block(chain_block).await.unwrap();
    }
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v0_to_v1_interrupted() {
    init_tracing();

    let (blocks, _faucet, _recipient) = load_test_vectors().unwrap();

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

        no_sync: false,
        no_db: false,
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

        no_sync: false,
        no_db: false,
    };

    let source = build_mockchain_source(blocks.clone());

    // Build v0 database.
    let zaino_db = ZainoDB::spawn(v0_config, source.clone()).await.unwrap();
    let mut parent_chain_work = ChainWork::from_u256(0.into());
    for (
        _h,
        zebra_block,
        (sapling_root, sapling_root_size, orchard_root, orchard_root_size),
        _treestates,
    ) in blocks.clone()
    {
        let chain_block = IndexedBlock::try_from((
            &zebra_block,
            sapling_root,
            sapling_root_size as u32,
            orchard_root,
            orchard_root_size as u32,
            &parent_chain_work,
            &zaino_common::Network::Regtest(ActivationHeights::default()).to_zebra_network(),
        ))
        .unwrap();
        parent_chain_work = *chain_block.chainwork();

        zaino_db.write_block(chain_block).await.unwrap();
    }
    zaino_db.wait_until_ready().await;
    dbg!(zaino_db.status());
    dbg!(zaino_db.db_height().await.unwrap());
    dbg!(zaino_db.shutdown().await.unwrap());

    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

    // Partial build v1 database.
    let zaino_db = DbBackend::spawn_v1(&v1_config).await.unwrap();
    let mut parent_chain_work = ChainWork::from_u256(0.into());
    for (
        h,
        zebra_block,
        (sapling_root, sapling_root_size, orchard_root, orchard_root_size),
        _treestates,
    ) in blocks.clone()
    {
        if h > 50 {
            break;
        }

        let chain_block = IndexedBlock::try_from((
            &zebra_block,
            sapling_root,
            sapling_root_size as u32,
            orchard_root,
            orchard_root_size as u32,
            &parent_chain_work,
            &zebra_chain::parameters::Network::new_regtest(
                zebra_chain::parameters::testnet::ConfiguredActivationHeights {
                    before_overwinter: Some(1),
                    overwinter: Some(1),
                    sapling: Some(1),
                    blossom: Some(1),
                    heartwood: Some(1),
                    canopy: Some(1),
                    nu5: Some(1),
                    nu6: Some(1),
                    // see https://zips.z.cash/#nu6-1-candidate-zips for info on NU6.1
                    nu6_1: None,
                    nu7: None,
                },
            ),
        ))
        .unwrap();

        parent_chain_work = *chain_block.index().chainwork();

        zaino_db.write_block(chain_block).await.unwrap();
    }
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v0_to_v1_partial() {
    init_tracing();

    let (blocks, _faucet, _recipient) = load_test_vectors().unwrap();

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

        no_sync: false,
        no_db: false,
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

        no_sync: false,
        no_db: false,
    };

    let source = build_mockchain_source(blocks.clone());

    // Build v0 database.
    let zaino_db = ZainoDB::spawn(v0_config, source.clone()).await.unwrap();
    let mut parent_chain_work = ChainWork::from_u256(0.into());
    for (
        _h,
        zebra_block,
        (sapling_root, sapling_root_size, orchard_root, orchard_root_size),
        _treestates,
    ) in blocks.clone()
    {
        let chain_block = IndexedBlock::try_from((
            &zebra_block,
            sapling_root,
            sapling_root_size as u32,
            orchard_root,
            orchard_root_size as u32,
            &parent_chain_work,
            &zaino_common::Network::Regtest(ActivationHeights::default()).to_zebra_network(),
        ))
        .unwrap();
        parent_chain_work = *chain_block.chainwork();

        zaino_db.write_block(chain_block).await.unwrap();
    }

    zaino_db.wait_until_ready().await;
    dbg!(zaino_db.status());
    dbg!(zaino_db.db_height().await.unwrap());
    dbg!(zaino_db.shutdown().await.unwrap());

    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

    // Partial build v1 database.
    let zaino_db = DbBackend::spawn_v1(&v1_config).await.unwrap();

    let mut parent_chain_work = ChainWork::from_u256(0.into());

    for (
        _h,
        zebra_block,
        (sapling_root, sapling_root_size, orchard_root, orchard_root_size),
        _treestates,
    ) in blocks.clone()
    {
        let chain_block = IndexedBlock::try_from((
            &zebra_block,
            sapling_root,
            sapling_root_size as u32,
            orchard_root,
            orchard_root_size as u32,
            &parent_chain_work,
            &zebra_chain::parameters::Network::new_regtest(
                zebra_chain::parameters::testnet::ConfiguredActivationHeights {
                    before_overwinter: Some(1),
                    overwinter: Some(1),
                    sapling: Some(1),
                    blossom: Some(1),
                    heartwood: Some(1),
                    canopy: Some(1),
                    nu5: Some(1),
                    nu6: Some(1),
                    // see https://zips.z.cash/#nu6-1-candidate-zips for info on NU6.1
                    nu6_1: None,
                    nu7: None,
                },
            ),
        ))
        .unwrap();

        parent_chain_work = *chain_block.index().chainwork();

        zaino_db.write_block(chain_block).await.unwrap();
    }

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
