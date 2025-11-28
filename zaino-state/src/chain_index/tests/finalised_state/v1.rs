//! Holds tests for the V1 database.

use std::path::PathBuf;
use tempfile::TempDir;

use zaino_common::network::ActivationHeights;
use zaino_common::{DatabaseConfig, Network, StorageConfig};

use crate::chain_index::finalised_state::capability::IndexedBlockExt;
use crate::chain_index::finalised_state::db::DbBackend;
use crate::chain_index::finalised_state::reader::DbReader;
use crate::chain_index::finalised_state::ZainoDB;
use crate::chain_index::source::test::MockchainSource;
use crate::chain_index::tests::init_tracing;
use crate::chain_index::tests::vectors::{
    build_mockchain_source, load_test_vectors, TestVectorBlockData, TestVectorData,
};
use crate::chain_index::types::TransactionHash;
use crate::error::FinalisedStateError;
use crate::{
    AddrScript, BlockCacheConfig, BlockMetadata, BlockWithMetadata, ChainWork, Height,
    IndexedBlock, Outpoint,
};

pub(crate) async fn spawn_v1_zaino_db(
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
        db_version: 1,
        network: Network::Regtest(ActivationHeights::default()),
    };

    let zaino_db = ZainoDB::spawn(config, source).await.unwrap();

    Ok((temp_dir, zaino_db))
}

pub(crate) async fn load_vectors_and_spawn_and_sync_v1_zaino_db(
) -> (TestVectorData, TempDir, ZainoDB) {
    let test_vector_data = load_test_vectors().unwrap();
    let blocks = test_vector_data.blocks.clone();

    dbg!(blocks.len());

    let source = build_mockchain_source(blocks.clone());

    let (db_dir, zaino_db) = spawn_v1_zaino_db(source).await.unwrap();

    crate::chain_index::tests::vectors::sync_db_with_blockdata(zaino_db.router(), blocks, None)
        .await;

    (test_vector_data, db_dir, zaino_db)
}

pub(crate) async fn load_vectors_v1db_and_reader(
) -> (TestVectorData, TempDir, std::sync::Arc<ZainoDB>, DbReader) {
    let (test_vector_data, db_dir, zaino_db) = load_vectors_and_spawn_and_sync_v1_zaino_db().await;

    let zaino_db = std::sync::Arc::new(zaino_db);

    zaino_db.wait_until_ready().await;
    dbg!(zaino_db.status());
    dbg!(zaino_db.db_height().await.unwrap()).unwrap();

    let db_reader = zaino_db.to_reader();
    dbg!(db_reader.db_height().await.unwrap()).unwrap();

    (test_vector_data, db_dir, zaino_db, db_reader)
}

// *** ZainoDB Tests ***

#[tokio::test(flavor = "multi_thread")]
async fn sync_to_height() {
    init_tracing();

    let blocks = load_test_vectors().unwrap().blocks;

    let source = build_mockchain_source(blocks.clone());

    let (_db_dir, zaino_db) = spawn_v1_zaino_db(source.clone()).await.unwrap();

    zaino_db.sync_to_height(Height(200), source).await.unwrap();

    zaino_db.wait_until_ready().await;
    dbg!(zaino_db.status());
    let built_db_height = dbg!(zaino_db.db_height().await.unwrap()).unwrap();

    assert_eq!(built_db_height, Height(200));
}

#[tokio::test(flavor = "multi_thread")]
async fn add_blocks_to_db_and_verify() {
    init_tracing();

    let (_test_vector_data, _db_dir, zaino_db) =
        load_vectors_and_spawn_and_sync_v1_zaino_db().await;
    zaino_db.wait_until_ready().await;
    dbg!(zaino_db.status());
    dbg!(zaino_db.db_height().await.unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_blocks_from_db() {
    init_tracing();

    let (_test_vector_data, _db_dir, zaino_db) =
        load_vectors_and_spawn_and_sync_v1_zaino_db().await;

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

#[tokio::test(flavor = "multi_thread")]
async fn save_db_to_file_and_reload() {
    init_tracing();

    let blocks = load_test_vectors().unwrap().blocks;

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
        db_version: 1,
        network: Network::Regtest(ActivationHeights::default()),
    };

    let source = build_mockchain_source(blocks.clone());
    let source_clone = source.clone();

    let config_clone = config.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            let zaino_db = ZainoDB::spawn(config_clone, source).await.unwrap();

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

#[tokio::test(flavor = "multi_thread")]
async fn load_db_backend_from_file() {
    init_tracing();

    let db_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("chain_index")
        .join("tests")
        .join("vectors")
        .join("v1_test_db");
    let config = BlockCacheConfig {
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
    let finalized_state_backend = DbBackend::spawn_v1(&config).await.unwrap();

    let mut prev_hash = None;
    for height in 0..=100 {
        let block = finalized_state_backend
            .get_chain_block(Height(height))
            .await
            .unwrap()
            .unwrap();
        if let Some(prev_hash) = prev_hash {
            assert_eq!(prev_hash, block.index().parent_hash);
        }
        prev_hash = Some(block.index().hash);
        assert_eq!(block.index.height, Some(Height(height)));
    }
    assert!(finalized_state_backend
        .get_chain_block(Height(101))
        .await
        .unwrap()
        .is_none());
    std::fs::remove_file(db_path.join("regtest").join("v1").join("lock.mdb")).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn try_write_invalid_block() {
    init_tracing();

    let (TestVectorData { blocks, .. }, _db_dir, zaino_db) =
        load_vectors_and_spawn_and_sync_v1_zaino_db().await;

    zaino_db.wait_until_ready().await;
    dbg!(zaino_db.status());
    dbg!(zaino_db.db_height().await.unwrap());

    let TestVectorBlockData {
        height,
        zebra_block,
        sapling_root,
        sapling_tree_size,
        orchard_root,
        orchard_tree_size,
        ..
    } = blocks.last().unwrap().clone();

    // NOTE: Currently using default here.
    let parent_chain_work = ChainWork::from_u256(0.into());
    let metadata = BlockMetadata::new(
        sapling_root,
        sapling_tree_size as u32,
        orchard_root,
        orchard_tree_size as u32,
        parent_chain_work,
        zaino_common::Network::Regtest(ActivationHeights::default()).to_zebra_network(),
    );

    let mut chain_block =
        IndexedBlock::try_from(BlockWithMetadata::new(&zebra_block, metadata)).unwrap();

    chain_block.index.height = Some(crate::Height(height + 1));
    dbg!(chain_block.index.height);

    let db_err = dbg!(zaino_db.write_block(chain_block).await);

    // TODO: Update with concrete err type.
    assert!(db_err.is_err());

    dbg!(zaino_db.db_height().await.unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn try_delete_block_with_invalid_height() {
    init_tracing();

    let (TestVectorData { blocks, .. }, _db_dir, zaino_db) =
        load_vectors_and_spawn_and_sync_v1_zaino_db().await;

    zaino_db.wait_until_ready().await;
    dbg!(zaino_db.status());
    dbg!(zaino_db.db_height().await.unwrap());

    let height = blocks.last().unwrap().clone().height;

    let delete_height = height - 1;

    let db_err = dbg!(
        zaino_db
            .delete_block_at_height(crate::Height(delete_height))
            .await
    );

    // TODO: Update with concrete err type.
    assert!(db_err.is_err());

    dbg!(zaino_db.db_height().await.unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn create_db_reader() {
    let (TestVectorData { blocks, .. }, _db_dir, zaino_db, db_reader) =
        load_vectors_v1db_and_reader().await;

    let data_height = blocks.last().unwrap().height;
    let db_height = dbg!(zaino_db.db_height().await.unwrap()).unwrap();
    let db_reader_height = dbg!(db_reader.db_height().await.unwrap()).unwrap();

    assert_eq!(data_height, db_height.0);
    assert_eq!(db_height, db_reader_height);
}

// *** DbReader Tests ***

#[tokio::test(flavor = "multi_thread")]
async fn get_chain_blocks() {
    init_tracing();

    let (TestVectorData { blocks, .. }, _db_dir, _zaino_db, db_reader) =
        load_vectors_v1db_and_reader().await;

    let mut parent_chain_work = ChainWork::from_u256(0.into());

    for TestVectorBlockData {
        height,
        zebra_block,
        sapling_root,
        sapling_tree_size,
        orchard_root,
        orchard_tree_size,
        ..
    } in blocks.iter()
    {
        let metadata = BlockMetadata::new(
            *sapling_root,
            *sapling_tree_size as u32,
            *orchard_root,
            *orchard_tree_size as u32,
            parent_chain_work,
            zebra_chain::parameters::Network::new_regtest(
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
        );

        let block_with_metadata = BlockWithMetadata::new(zebra_block, metadata);
        let chain_block = IndexedBlock::try_from(block_with_metadata).unwrap();

        parent_chain_work = *chain_block.index().chainwork();

        let reader_chain_block = db_reader.get_chain_block(Height(*height)).await.unwrap();
        assert_eq!(Some(chain_block), reader_chain_block);
        println!("IndexedBlock at height {height} OK");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn get_compact_blocks() {
    init_tracing();

    let (TestVectorData { blocks, .. }, _db_dir, _zaino_db, db_reader) =
        load_vectors_v1db_and_reader().await;

    let mut parent_chain_work = ChainWork::from_u256(0.into());

    for TestVectorBlockData {
        height,
        zebra_block,
        sapling_root,
        sapling_tree_size,
        orchard_root,
        orchard_tree_size,
        ..
    } in blocks.iter()
    {
        let metadata = BlockMetadata::new(
            *sapling_root,
            *sapling_tree_size as u32,
            *orchard_root,
            *orchard_tree_size as u32,
            parent_chain_work,
            zebra_chain::parameters::Network::new_regtest(
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
        );

        let block_with_metadata = BlockWithMetadata::new(zebra_block, metadata);
        let chain_block = IndexedBlock::try_from(block_with_metadata).unwrap();
        let compact_block = chain_block.to_compact_block();

        parent_chain_work = *chain_block.index().chainwork();

        let reader_compact_block = db_reader.get_compact_block(Height(*height)).await.unwrap();
        assert_eq!(compact_block, reader_compact_block);
        println!("CompactBlock at height {height} OK");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn get_faucet_txids() {
    init_tracing();

    let (TestVectorData { blocks, faucet, .. }, _db_dir, _zaino_db, db_reader) =
        load_vectors_v1db_and_reader().await;

    let start = Height(blocks.first().unwrap().height);
    let end = Height(blocks.last().unwrap().height);
    dbg!(&start, &end);

    let (_faucet_address, _txid, _output_index, faucet_script, _satoshis, _height) =
        faucet.utxos.first().unwrap().into_parts();
    let faucet_addr_script = AddrScript::from_script(faucet_script.as_raw_bytes())
        .expect("faucet script must be standard P2PKH or P2SH");

    let mut parent_chain_work = ChainWork::from_u256(0.into());

    for TestVectorBlockData {
        height,
        zebra_block,
        sapling_root,
        sapling_tree_size,
        orchard_root,
        orchard_tree_size,
        ..
    } in blocks.iter()
    {
        let metadata = BlockMetadata::new(
            *sapling_root,
            *sapling_tree_size as u32,
            *orchard_root,
            *orchard_tree_size as u32,
            parent_chain_work,
            zebra_chain::parameters::Network::new_regtest(
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
        );

        let block_with_metadata = BlockWithMetadata::new(zebra_block, metadata);
        let chain_block = IndexedBlock::try_from(block_with_metadata).unwrap();

        parent_chain_work = *chain_block.index().chainwork();

        println!("Checking faucet txids at height {height}");
        let block_height = Height(*height);
        let block_txids: Vec<String> = chain_block
            .transactions()
            .iter()
            .map(|tx_data| tx_data.txid().to_string())
            .collect();
        let filtered_block_txids: Vec<String> = block_txids
            .into_iter()
            .filter(|txid| faucet.txids.contains(txid))
            .collect();
        dbg!(&filtered_block_txids);

        let reader_faucet_tx_locations = db_reader
            .addr_tx_locations_by_range(faucet_addr_script, block_height, block_height)
            .await
            .unwrap()
            .unwrap_or_default();
        let mut reader_block_txids = Vec::new();
        for tx_location in reader_faucet_tx_locations {
            let txid = db_reader.get_txid(tx_location).await.unwrap();
            reader_block_txids.push(txid.to_string());
        }
        dbg!(&reader_block_txids);

        assert_eq!(filtered_block_txids.len(), reader_block_txids.len());
        assert_eq!(filtered_block_txids, reader_block_txids);
    }

    println!("Checking full faucet data");
    let reader_faucet_tx_locations = db_reader
        .addr_tx_locations_by_range(faucet_addr_script, start, end)
        .await
        .unwrap()
        .unwrap();
    let mut reader_faucet_txids = Vec::new();
    for tx_location in reader_faucet_tx_locations {
        let txid = db_reader.get_txid(tx_location).await.unwrap();
        reader_faucet_txids.push(txid.to_string());
    }

    assert_eq!(faucet.txids.len(), reader_faucet_txids.len());
    assert_eq!(faucet.txids, reader_faucet_txids);
}

#[tokio::test(flavor = "multi_thread")]
async fn get_recipient_txids() {
    init_tracing();

    let (
        TestVectorData {
            blocks, recipient, ..
        },
        _db_dir,
        _zaino_db,
        db_reader,
    ) = load_vectors_v1db_and_reader().await;

    let start = Height(blocks.first().unwrap().height);
    let end = Height(blocks.last().unwrap().height);

    let (_recipient_address, _txid, _output_index, recipient_script, _satoshis, _height) =
        recipient.utxos.first().unwrap().into_parts();
    let recipient_addr_script = AddrScript::from_script(recipient_script.as_raw_bytes())
        .expect("faucet script must be standard P2PKH or P2SH");

    let mut parent_chain_work = ChainWork::from_u256(0.into());

    for TestVectorBlockData {
        height,
        zebra_block,
        sapling_root,
        sapling_tree_size,
        orchard_root,
        orchard_tree_size,
        ..
    } in blocks.iter()
    {
        let metadata = BlockMetadata::new(
            *sapling_root,
            *sapling_tree_size as u32,
            *orchard_root,
            *orchard_tree_size as u32,
            parent_chain_work,
            zebra_chain::parameters::Network::new_regtest(
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
        );

        let block_with_metadata = BlockWithMetadata::new(zebra_block, metadata);
        let chain_block = IndexedBlock::try_from(block_with_metadata).unwrap();

        parent_chain_work = *chain_block.index().chainwork();

        println!("Checking recipient txids at height {height}");
        let block_height = Height(*height);
        let block_txids: Vec<String> = chain_block
            .transactions()
            .iter()
            .map(|tx_data| tx_data.txid().to_string())
            .collect();

        // Get block txids that are relevant to recipient.
        let filtered_block_txids: Vec<String> = block_txids
            .into_iter()
            .filter(|txid| recipient.txids.contains(txid))
            .collect();
        dbg!(&filtered_block_txids);

        let reader_recipient_tx_locations = match db_reader
            .addr_tx_locations_by_range(recipient_addr_script, block_height, block_height)
            .await
            .unwrap()
        {
            Some(v) => v,
            None => continue,
        };
        let mut reader_block_txids = Vec::new();
        for tx_location in reader_recipient_tx_locations {
            let txid = db_reader.get_txid(tx_location).await.unwrap();
            reader_block_txids.push(txid.to_string());
        }
        dbg!(&reader_block_txids);

        assert_eq!(filtered_block_txids.len(), reader_block_txids.len());
        assert_eq!(filtered_block_txids, reader_block_txids);
    }

    println!("Checking full faucet data");
    let reader_recipient_tx_locations = db_reader
        .addr_tx_locations_by_range(recipient_addr_script, start, end)
        .await
        .unwrap()
        .unwrap();

    let mut reader_recipient_txids = Vec::new();
    for tx_location in reader_recipient_tx_locations {
        let txid = db_reader.get_txid(tx_location).await.unwrap();
        reader_recipient_txids.push(txid.to_string());
    }

    assert_eq!(recipient.txids.len(), reader_recipient_txids.len());
    assert_eq!(recipient.txids, reader_recipient_txids);
}

#[tokio::test(flavor = "multi_thread")]
async fn get_faucet_utxos() {
    init_tracing();

    let (TestVectorData { blocks, faucet, .. }, _db_dir, _zaino_db, db_reader) =
        load_vectors_v1db_and_reader().await;

    let start = Height(blocks.first().unwrap().height);
    let end = Height(blocks.last().unwrap().height);

    let (_faucet_address, _txid, _output_index, faucet_script, _satoshis, _height) =
        faucet.utxos.first().unwrap().into_parts();
    let faucet_addr_script = AddrScript::from_script(faucet_script.as_raw_bytes())
        .expect("faucet script must be standard P2PKH or P2SH");

    let mut cleaned_utxos = Vec::new();
    for utxo in faucet.utxos.iter() {
        let (_faucet_address, txid, output_index, _faucet_script, satoshis, _height) =
            utxo.into_parts();
        cleaned_utxos.push((txid.to_string(), output_index.index(), satoshis));
    }

    let reader_faucet_utxo_indexes = db_reader
        .addr_utxos_by_range(faucet_addr_script, start, end)
        .await
        .unwrap()
        .unwrap();

    let mut reader_faucet_utxos = Vec::new();

    for (tx_location, vout, value) in reader_faucet_utxo_indexes {
        let txid = db_reader.get_txid(tx_location).await.unwrap().to_string();
        reader_faucet_utxos.push((txid, vout as u32, value));
    }

    assert_eq!(cleaned_utxos.len(), reader_faucet_utxos.len());
    assert_eq!(cleaned_utxos, reader_faucet_utxos);
}

#[tokio::test(flavor = "multi_thread")]
async fn get_recipient_utxos() {
    init_tracing();

    let (
        TestVectorData {
            blocks, recipient, ..
        },
        _db_dir,
        _zaino_db,
        db_reader,
    ) = load_vectors_v1db_and_reader().await;

    let start = Height(blocks.first().unwrap().height);
    let end = Height(blocks.last().unwrap().height);

    let (_recipient_address, _txid, _output_index, recipient_script, _satoshis, _height) =
        recipient.utxos.first().unwrap().into_parts();
    let recipient_addr_script = AddrScript::from_script(recipient_script.as_raw_bytes())
        .expect("faucet script must be standard P2PKH or P2SH");

    let mut cleaned_utxos = Vec::new();
    for utxo in recipient.utxos.iter() {
        let (_recipient_address, txid, output_index, _recipient_script, satoshis, _height) =
            utxo.into_parts();
        cleaned_utxos.push((txid.to_string(), output_index.index(), satoshis));
    }

    let reader_recipient_utxo_indexes = db_reader
        .addr_utxos_by_range(recipient_addr_script, start, end)
        .await
        .unwrap()
        .unwrap();

    let mut reader_recipient_utxos = Vec::new();

    for (tx_location, vout, value) in reader_recipient_utxo_indexes {
        let txid = db_reader.get_txid(tx_location).await.unwrap().to_string();
        reader_recipient_utxos.push((txid, vout as u32, value));
    }

    assert_eq!(cleaned_utxos.len(), reader_recipient_utxos.len());
    assert_eq!(cleaned_utxos, reader_recipient_utxos);
}

#[tokio::test(flavor = "multi_thread")]
async fn get_balance() {
    init_tracing();

    let (test_vector_data, _db_dir, _zaino_db, db_reader) = load_vectors_v1db_and_reader().await;

    let start = Height(test_vector_data.blocks.first().unwrap().height);
    let end = Height(test_vector_data.blocks.last().unwrap().height);

    // Check faucet

    let (_faucet_address, _txid, _output_index, faucet_script, _satoshis, _height) =
        test_vector_data.faucet.utxos.first().unwrap().into_parts();
    let faucet_addr_script = AddrScript::from_script(faucet_script.as_raw_bytes())
        .expect("faucet script must be standard P2PKH or P2SH");

    let reader_faucet_balance = dbg!(db_reader
        .addr_balance_by_range(faucet_addr_script, start, end)
        .await
        .unwrap()) as u64;

    assert_eq!(test_vector_data.faucet.balance, reader_faucet_balance);

    // Check recipient

    let (_recipient_address, _txid, _output_index, recipient_script, _satoshis, _height) =
        test_vector_data
            .recipient
            .utxos
            .first()
            .unwrap()
            .into_parts();
    let recipient_addr_script = AddrScript::from_script(recipient_script.as_raw_bytes())
        .expect("faucet script must be standard P2PKH or P2SH");

    let reader_recipient_balance = dbg!(db_reader
        .addr_balance_by_range(recipient_addr_script, start, end)
        .await
        .unwrap()) as u64;

    assert_eq!(test_vector_data.recipient.balance, reader_recipient_balance);
}

#[tokio::test(flavor = "multi_thread")]
async fn check_faucet_spent_map() {
    init_tracing();

    let (TestVectorData { blocks, faucet, .. }, _db_dir, _zaino_db, db_reader) =
        load_vectors_v1db_and_reader().await;

    let (_faucet_address, _txid, _output_index, faucet_script, _satoshis, _height) =
        faucet.utxos.first().unwrap().into_parts();
    let faucet_addr_script = AddrScript::from_script(faucet_script.as_raw_bytes())
        .expect("faucet script must be standard P2PKH or P2SH");

    // collect faucet outpoints
    let mut faucet_outpoints = Vec::new();
    let mut faucet_ouptpoints_spent_status = Vec::new();

    let mut parent_chain_work = ChainWork::from_u256(0.into());

    for TestVectorBlockData {
        zebra_block,
        sapling_root,
        sapling_tree_size,
        orchard_root,
        orchard_tree_size,
        ..
    } in blocks.iter()
    {
        let metadata = BlockMetadata::new(
            *sapling_root,
            *sapling_tree_size as u32,
            *orchard_root,
            *orchard_tree_size as u32,
            parent_chain_work,
            zebra_chain::parameters::Network::new_regtest(
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
        );

        let block_with_metadata = BlockWithMetadata::new(zebra_block, metadata);
        let chain_block = IndexedBlock::try_from(block_with_metadata).unwrap();

        parent_chain_work = *chain_block.index().chainwork();

        for tx in chain_block.transactions() {
            let txid = tx.txid().0;
            let outputs = tx.transparent().outputs();
            for (vout_idx, output) in outputs.iter().enumerate() {
                if output.script_hash() == faucet_addr_script.hash() {
                    let outpoint = Outpoint::new(txid, vout_idx as u32);

                    let spender = db_reader.get_outpoint_spender(outpoint).await.unwrap();

                    faucet_outpoints.push(outpoint);
                    faucet_ouptpoints_spent_status.push(spender);
                }
            }
        }
    }

    // collect faucet txids holding utxos
    let mut faucet_utxo_indexes = Vec::new();
    for utxo in faucet.utxos.iter() {
        let (_faucet_address, txid, output_index, _faucet_script, _satoshis, _height) =
            utxo.into_parts();
        faucet_utxo_indexes.push((txid.to_string(), output_index.index()));
    }

    // check full spent outpoints map
    let faucet_spent_map = db_reader
        .get_outpoint_spenders(faucet_outpoints.clone())
        .await
        .unwrap();
    assert_eq!(&faucet_ouptpoints_spent_status, &faucet_spent_map);

    for (outpoint, spender_option) in faucet_outpoints
        .iter()
        .zip(faucet_ouptpoints_spent_status.iter())
    {
        let outpoint_tuple = (
            TransactionHash::from(*outpoint.prev_txid()).to_string(),
            outpoint.prev_index(),
        );
        match spender_option {
            Some(spender_index) => {
                let spender_tx = blocks.iter().find_map(
                    |TestVectorBlockData {
                         zebra_block,
                         sapling_root,
                         sapling_tree_size,
                         orchard_root,
                         orchard_tree_size,
                         ..
                     }| {
                        // NOTE: Currently using default here.
                        let parent_chain_work = ChainWork::from_u256(0.into());
                        let metadata = BlockMetadata::new(
                            *sapling_root,
                            *sapling_tree_size as u32,
                            *orchard_root,
                            *orchard_tree_size as u32,
                            parent_chain_work,
                            zaino_common::Network::Regtest(ActivationHeights::default())
                                .to_zebra_network(),
                        );
                        let chain_block =
                            IndexedBlock::try_from(BlockWithMetadata::new(zebra_block, metadata))
                                .unwrap();

                        chain_block
                            .transactions()
                            .iter()
                            .find(|tx| {
                                let (block_height, tx_idx) =
                                    (spender_index.block_height(), spender_index.tx_index());
                                chain_block.index().height() == Some(Height(block_height))
                                    && tx.index() == tx_idx as u64
                            })
                            .cloned()
                    },
                );
                assert!(
                    spender_tx.is_some(),
                    "Spender transaction not found in blocks!"
                );

                let spender_tx = spender_tx.unwrap();
                let matches = spender_tx.transparent().inputs().iter().any(|input| {
                    input.prevout_txid() == outpoint.prev_txid()
                        && input.prevout_index() == outpoint.prev_index()
                });
                assert!(
                    matches,
                    "Spender transaction does not actually spend the outpoint: {outpoint:?}"
                );

                assert!(
                    !faucet_utxo_indexes.contains(&outpoint_tuple),
                    "Spent outpoint should NOT be in UTXO set, but found: {outpoint_tuple:?}"
                );
            }
            None => {
                assert!(
                    faucet_utxo_indexes.contains(&outpoint_tuple),
                    "Unspent outpoint should be in UTXO set, but NOT found: {outpoint_tuple:?}"
                );
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn check_recipient_spent_map() {
    init_tracing();

    let (
        TestVectorData {
            blocks, recipient, ..
        },
        _db_dir,
        _zaino_db,
        db_reader,
    ) = load_vectors_v1db_and_reader().await;

    let (_recipient_address, _txid, _output_index, recipient_script, _satoshis, _height) =
        recipient.utxos.first().unwrap().into_parts();
    let recipient_addr_script = AddrScript::from_script(recipient_script.as_raw_bytes())
        .expect("faucet script must be standard P2PKH or P2SH");

    // collect faucet outpoints
    let mut recipient_outpoints = Vec::new();
    let mut recipient_ouptpoints_spent_status = Vec::new();

    let mut parent_chain_work = ChainWork::from_u256(0.into());

    for TestVectorBlockData {
        zebra_block,
        sapling_root,
        sapling_tree_size,
        orchard_root,
        orchard_tree_size,
        ..
    } in blocks.iter()
    {
        let metadata = BlockMetadata::new(
            *sapling_root,
            *sapling_tree_size as u32,
            *orchard_root,
            *orchard_tree_size as u32,
            parent_chain_work,
            zebra_chain::parameters::Network::new_regtest(
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
        );

        let block_with_metadata = BlockWithMetadata::new(zebra_block, metadata);
        let chain_block = IndexedBlock::try_from(block_with_metadata).unwrap();

        parent_chain_work = *chain_block.index().chainwork();

        for tx in chain_block.transactions() {
            let txid = tx.txid().0;
            let outputs = tx.transparent().outputs();
            for (vout_idx, output) in outputs.iter().enumerate() {
                if output.script_hash() == recipient_addr_script.hash() {
                    let outpoint = Outpoint::new(txid, vout_idx as u32);

                    let spender = db_reader.get_outpoint_spender(outpoint).await.unwrap();

                    recipient_outpoints.push(outpoint);
                    recipient_ouptpoints_spent_status.push(spender);
                }
            }
        }
    }

    // collect faucet txids holding utxos
    let mut recipient_utxo_indexes = Vec::new();
    for utxo in recipient.utxos.iter() {
        let (_recipient_address, txid, output_index, _recipient_script, _satoshis, _height) =
            utxo.into_parts();
        recipient_utxo_indexes.push((txid.to_string(), output_index.index()));
    }

    // check full spent outpoints map
    let recipient_spent_map = db_reader
        .get_outpoint_spenders(recipient_outpoints.clone())
        .await
        .unwrap();
    assert_eq!(&recipient_ouptpoints_spent_status, &recipient_spent_map);

    for (outpoint, spender_option) in recipient_outpoints
        .iter()
        .zip(recipient_ouptpoints_spent_status.iter())
    {
        let outpoint_tuple = (
            TransactionHash::from(*outpoint.prev_txid()).to_string(),
            outpoint.prev_index(),
        );
        match spender_option {
            Some(spender_index) => {
                let spender_tx = blocks.iter().find_map(
                    |TestVectorBlockData {
                         zebra_block,
                         sapling_root,
                         sapling_tree_size,
                         orchard_root,
                         orchard_tree_size,
                         ..
                     }| {
                        // NOTE: Currently using default here.
                        let parent_chain_work = ChainWork::from_u256(0.into());
                        let metadata = BlockMetadata::new(
                            *sapling_root,
                            *sapling_tree_size as u32,
                            *orchard_root,
                            *orchard_tree_size as u32,
                            parent_chain_work,
                            zaino_common::Network::Regtest(ActivationHeights::default())
                                .to_zebra_network(),
                        );
                        let chain_block =
                            IndexedBlock::try_from(BlockWithMetadata::new(zebra_block, metadata))
                                .unwrap();

                        chain_block
                            .transactions()
                            .iter()
                            .find(|tx| {
                                let (block_height, tx_idx) =
                                    (spender_index.block_height(), spender_index.tx_index());
                                chain_block.index().height() == Some(Height(block_height))
                                    && tx.index() == tx_idx as u64
                            })
                            .cloned()
                    },
                );
                assert!(
                    spender_tx.is_some(),
                    "Spender transaction not found in blocks!"
                );

                let spender_tx = spender_tx.unwrap();
                let matches = spender_tx.transparent().inputs().iter().any(|input| {
                    input.prevout_txid() == outpoint.prev_txid()
                        && input.prevout_index() == outpoint.prev_index()
                });
                assert!(
                    matches,
                    "Spender transaction does not actually spend the outpoint: {outpoint:?}"
                );

                assert!(
                    !recipient_utxo_indexes.contains(&outpoint_tuple),
                    "Spent outpoint should NOT be in UTXO set, but found: {outpoint_tuple:?}"
                );
            }
            None => {
                assert!(
                    recipient_utxo_indexes.contains(&outpoint_tuple),
                    "Unspent outpoint should be in UTXO set, but NOT found: {outpoint_tuple:?}"
                );
            }
        }
    }
}
