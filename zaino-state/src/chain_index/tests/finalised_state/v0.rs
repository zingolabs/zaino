//! Holds tests for the V0 database.

use std::path::PathBuf;
use tempfile::TempDir;

use zaino_common::network::ActivationHeights;
use zaino_common::{DatabaseConfig, Network, StorageConfig};
use zaino_proto::proto::compact_formats::CompactBlock;
use zebra_rpc::methods::GetAddressUtxos;

use crate::chain_index::finalised_state::reader::DbReader;
use crate::chain_index::finalised_state::ZainoDB;
use crate::chain_index::source::test::MockchainSource;
use crate::chain_index::tests::init_tracing;
use crate::chain_index::tests::vectors::{build_mockchain_source, load_test_vectors};
use crate::error::FinalisedStateError;
use crate::{BlockCacheConfig, ChainWork, Height, IndexedBlock};

pub(crate) async fn spawn_v0_zaino_db(
    source: MockchainSource,
) -> Result<(TempDir, ZainoDB), FinalisedStateError> {
    let temp_dir: TempDir = tempfile::tempdir().unwrap();
    let db_path: PathBuf = temp_dir.path().to_path_buf();

    let config = BlockCacheConfig {
        storage: StorageConfig {
            database: DatabaseConfig {
                path: db_path,
                ..Default::default()
            },
            ..Default::default()
        },
        db_version: 0,
        network: Network::Regtest(ActivationHeights::default()),

        no_sync: false,
        no_db: false,
    };

    let zaino_db = ZainoDB::spawn(config, source).await.unwrap();

    Ok((temp_dir, zaino_db))
}

pub(crate) async fn load_vectors_and_spawn_and_sync_v0_zaino_db() -> (
    Vec<(
        u32,
        IndexedBlock,
        CompactBlock,
        zebra_chain::block::Block,
        (
            zebra_chain::sapling::tree::Root,
            u64,
            zebra_chain::orchard::tree::Root,
            u64,
        ),
    )>,
    (Vec<String>, Vec<GetAddressUtxos>, u64),
    (Vec<String>, Vec<GetAddressUtxos>, u64),
    TempDir,
    ZainoDB,
) {
    let (blocks, faucet, recipient) = load_test_vectors().unwrap();

    let source = build_mockchain_source(blocks.clone());

    let (db_dir, zaino_db) = spawn_v0_zaino_db(source).await.unwrap();

    let mut parent_chain_work = ChainWork::from_u256(0.into());

    for (
        _h,
        _chain_block,
        _compact_block,
        zebra_block,
        (sapling_root, sapling_root_size, orchard_root, orchard_root_size),
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
    (blocks, faucet, recipient, db_dir, zaino_db)
}

pub(crate) async fn load_vectors_v0db_and_reader() -> (
    Vec<(
        u32,
        IndexedBlock,
        CompactBlock,
        zebra_chain::block::Block,
        (
            zebra_chain::sapling::tree::Root,
            u64,
            zebra_chain::orchard::tree::Root,
            u64,
        ),
    )>,
    (Vec<String>, Vec<GetAddressUtxos>, u64),
    (Vec<String>, Vec<GetAddressUtxos>, u64),
    TempDir,
    std::sync::Arc<ZainoDB>,
    DbReader,
) {
    let (blocks, faucet, recipient, db_dir, zaino_db) =
        load_vectors_and_spawn_and_sync_v0_zaino_db().await;

    let zaino_db = std::sync::Arc::new(zaino_db);

    zaino_db.wait_until_ready().await;
    dbg!(zaino_db.status());
    dbg!(zaino_db.db_height().await.unwrap()).unwrap();

    let db_reader = zaino_db.to_reader();
    dbg!(db_reader.db_height().await.unwrap()).unwrap();

    (blocks, faucet, recipient, db_dir, zaino_db, db_reader)
}

// *** ZainoDB Tests ***

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_to_height() {
    init_tracing();

    let (blocks, _faucet, _recipient) = load_test_vectors().unwrap();

    let source = build_mockchain_source(blocks.clone());

    let (_db_dir, zaino_db) = spawn_v0_zaino_db(source.clone()).await.unwrap();

    zaino_db.sync_to_height(Height(200), source).await.unwrap();

    zaino_db.wait_until_ready().await;
    dbg!(zaino_db.status());
    let built_db_height = dbg!(zaino_db.db_height().await.unwrap()).unwrap();

    assert_eq!(built_db_height, Height(200));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_blocks_to_db_and_verify() {
    init_tracing();

    let (_blocks, _faucet, _recipient, _db_dir, zaino_db) =
        load_vectors_and_spawn_and_sync_v0_zaino_db().await;
    zaino_db.wait_until_ready().await;
    dbg!(zaino_db.status());
    dbg!(zaino_db.db_height().await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_blocks_from_db() {
    init_tracing();

    let (_blocks, _faucet, _recipient, _db_dir, zaino_db) =
        load_vectors_and_spawn_and_sync_v0_zaino_db().await;

    for h in (1..=200).rev() {
        // dbg!("Deleting block at height {}", h);
        zaino_db
            .delete_block_at_height(crate::Height(h))
            .await
            .unwrap();
    }

    zaino_db.wait_until_ready().await;
    dbg!(zaino_db.status());
    dbg!(zaino_db.db_height().await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn save_db_to_file_and_reload() {
    init_tracing();

    let (blocks, _faucet, _recipient) = load_test_vectors().unwrap();

    let temp_dir: TempDir = tempfile::tempdir().unwrap();
    let db_path: PathBuf = temp_dir.path().to_path_buf();
    let config = BlockCacheConfig {
        storage: StorageConfig {
            database: DatabaseConfig {
                path: db_path,
                ..Default::default()
            },
            ..Default::default()
        },
        db_version: 0,
        network: Network::Regtest(ActivationHeights::default()),

        no_sync: false,
        no_db: false,
    };

    let source = build_mockchain_source(blocks.clone());
    let source_clone = source.clone();

    let blocks_clone = blocks.clone();
    let config_clone = config.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            let zaino_db = ZainoDB::spawn(config_clone, source).await.unwrap();

            let mut parent_chain_work = ChainWork::from_u256(0.into());

            for (
                _h,
                _chain_block,
                _compact_block,
                zebra_block,
                (sapling_root, sapling_root_size, orchard_root, orchard_root_size),
            ) in blocks_clone
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

            zaino_db.wait_until_ready().await;
            dbg!(zaino_db.status());
            dbg!(zaino_db.db_height().await.unwrap());

            dbg!(zaino_db.shutdown().await.unwrap());
        });
    })
    .join()
    .unwrap();

    std::thread::sleep(std::time::Duration::from_millis(1000));

    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            dbg!(config
                .storage
                .database
                .path
                .read_dir()
                .unwrap()
                .collect::<Vec<_>>());
            let zaino_db_2 = ZainoDB::spawn(config, source_clone).await.unwrap();

            zaino_db_2.wait_until_ready().await;
            dbg!(zaino_db_2.status());
            let db_height = dbg!(zaino_db_2.db_height().await.unwrap()).unwrap();

            assert_eq!(db_height.0, 200);

            dbg!(zaino_db_2.shutdown().await.unwrap());
        });
    })
    .join()
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_db_reader() {
    init_tracing();

    let (blocks, _faucet, _recipient, _db_dir, zaino_db, db_reader) =
        load_vectors_v0db_and_reader().await;

    let (data_height, _, _, _, _) = blocks.last().unwrap();
    let db_height = dbg!(zaino_db.db_height().await.unwrap()).unwrap();
    let db_reader_height = dbg!(db_reader.db_height().await.unwrap()).unwrap();

    assert_eq!(data_height, &db_height.0);
    assert_eq!(db_height, db_reader_height);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_compact_blocks() {
    init_tracing();

    let (blocks, _faucet, _recipient, _db_dir, _zaino_db, db_reader) =
        load_vectors_v0db_and_reader().await;

    let mut parent_chain_work = ChainWork::from_u256(0.into());

    for (
        height,
        _chain_block,
        _compact_block,
        zebra_block,
        (sapling_root, sapling_root_size, orchard_root, orchard_root_size),
    ) in blocks.iter()
    {
        let chain_block = IndexedBlock::try_from((
            zebra_block,
            *sapling_root,
            *sapling_root_size as u32,
            *orchard_root,
            *orchard_root_size as u32,
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
        let compact_block = chain_block.to_compact_block();

        parent_chain_work = *chain_block.index().chainwork();

        let reader_compact_block = db_reader.get_compact_block(Height(*height)).await.unwrap();
        assert_eq!(compact_block, reader_compact_block);
        println!("CompactBlock at height {height} OK");
    }
}
