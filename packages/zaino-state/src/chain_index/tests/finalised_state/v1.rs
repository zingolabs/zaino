//! Holds tests for the V1 database.

use hex::ToHex;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;
use zaino_common::network::ActivationHeights;
use zaino_common::{DatabaseConfig, Network, StorageConfig};
use zaino_proto::proto::utils::{compact_block_with_pool_types, PoolTypeFilter};

use crate::chain_index::finalised_state::capability::IndexedBlockExt;
use crate::chain_index::finalised_state::finalised_source::FinalisedSource;
use crate::chain_index::finalised_state::reader::DbReader;
use crate::chain_index::finalised_state::FinalisedState;
use crate::chain_index::source::mockchain_source::MockchainSource;
use crate::chain_index::tests::init_tracing;
use crate::chain_index::tests::vectors::{
    build_mockchain_source, copy_dir_recursive, index_test_vector_blocks, indexed_block_chain,
    load_test_vectors, TestVectorBlockData, TestVectorData,
};

use crate::chain_index::types::TransactionHash;

use crate::chain_index::types::db::metadata::FinalisedTxOutSetInfoAccumulator;
use crate::error::FinalisedStateError;
use crate::{BlockMetadata, BlockWithMetadata, ChainIndexConfig, ChainWork, Height, IndexedBlock};

use crate::{AddrScript, Outpoint};

pub(crate) async fn spawn_v1_zaino_db(
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
        ephemeral: false,
        db_version: 1,
        network: Network::Regtest(ActivationHeights::default()),
    };

    let zaino_db = FinalisedState::spawn(config, source).await.unwrap();

    Ok((temp_dir, zaino_db))
}

pub(crate) async fn load_vectors_and_spawn_and_sync_v1_zaino_db(
) -> (TestVectorData, TempDir, FinalisedState<MockchainSource>) {
    let test_vector_data = load_test_vectors().unwrap();
    let blocks = test_vector_data.blocks.clone();

    dbg!(blocks.len());

    let source = build_mockchain_source(blocks.clone());

    let (db_dir, zaino_db) = spawn_v1_zaino_db(source).await.unwrap();

    crate::chain_index::tests::vectors::sync_db_with_blockdata(zaino_db.router(), blocks, None)
        .await;

    (test_vector_data, db_dir, zaino_db)
}

pub(crate) async fn load_vectors_v1db_and_reader() -> (
    TestVectorData,
    TempDir,
    std::sync::Arc<FinalisedState<MockchainSource>>,
    DbReader<MockchainSource>,
) {
    let (test_vector_data, db_dir, zaino_db) = load_vectors_and_spawn_and_sync_v1_zaino_db().await;

    let zaino_db = std::sync::Arc::new(zaino_db);

    zaino_db.wait_until_ready().await;
    dbg!(zaino_db.status());
    dbg!(zaino_db.db_height().await.unwrap()).unwrap();

    let db_reader = zaino_db.to_reader();
    dbg!(db_reader.db_height().await.unwrap()).unwrap();

    (test_vector_data, db_dir, zaino_db, db_reader)
}

// *** FinalisedState Tests ***

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_returns_promptly() {
    super::assert_shutdown_returns_promptly("DbV1", spawn_v1_zaino_db).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sync_to_height() {
    init_tracing();

    let blocks = load_test_vectors().unwrap().blocks;

    let source = build_mockchain_source(blocks.clone());

    let (_db_dir, zaino_db) = spawn_v1_zaino_db(source.clone()).await.unwrap();

    zaino_db.sync_to_height(Height(200), &source).await.unwrap();

    zaino_db.wait_until_synced().await;
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
    let config = ChainIndexConfig {
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
    let source_clone = source.clone();

    let config_clone = config.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            let zaino_db = FinalisedState::spawn(config_clone, source).await.unwrap();

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
            let zaino_db_2 = FinalisedState::spawn(config, source_clone).await.unwrap();

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

    let fixture_db_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("chain_index")
        .join("tests")
        .join("vectors")
        .join("v1_test_db");
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("v1_test_db");
    copy_dir_recursive(&fixture_db_path, &db_path).unwrap();

    let config = ChainIndexConfig {
        storage: StorageConfig {
            database: DatabaseConfig {
                path: db_path.clone(),
                ..Default::default()
            },
            ..Default::default()
        },
        ephemeral: false,
        db_version: 1,
        network: Network::Regtest(ActivationHeights::default()),
    };
    let finalized_state_backend: FinalisedSource<MockchainSource> =
        FinalisedSource::spawn_v1(&config).await.unwrap();

    let mut prev_hash = None;
    for height in 0..=100 {
        let block = finalized_state_backend
            .get_chain_block(Height(height))
            .await
            .unwrap()
            .unwrap();
        if let Some(prev_hash) = prev_hash {
            assert_eq!(prev_hash, block.context.parent_hash);
        }
        prev_hash = Some(block.context.index.hash);
        assert_eq!(block.context.index.height, Height(height));
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

    chain_block.context.index.height = crate::Height(height + 1);
    dbg!(chain_block.context.index.height);

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

    for chain_block in indexed_block_chain(&blocks) {
        let height = chain_block.context.index.height;
        let reader_chain_block = db_reader.get_chain_block_by_height(height).await.unwrap();
        assert_eq!(Some(chain_block), reader_chain_block);
        println!("IndexedBlock at height {} OK", height.0);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn get_compact_blocks() {
    init_tracing();

    let (TestVectorData { blocks, .. }, _db_dir, _zaino_db, db_reader) =
        load_vectors_v1db_and_reader().await;

    for chain_block in indexed_block_chain(&blocks) {
        let height = chain_block.context.index.height;
        let compact_block = chain_block.to_compact_block();

        let reader_compact_block_default = db_reader
            .get_compact_block(height, PoolTypeFilter::default())
            .await
            .unwrap();
        let default_compact_block = compact_block_with_pool_types(
            compact_block.clone(),
            &PoolTypeFilter::default().to_pool_types_vector(),
        );
        assert_eq!(default_compact_block, reader_compact_block_default);

        let reader_compact_block_all_data = db_reader
            .get_compact_block(height, PoolTypeFilter::includes_all())
            .await
            .unwrap();
        let all_data_compact_block = compact_block_with_pool_types(
            compact_block,
            &PoolTypeFilter::includes_all().to_pool_types_vector(),
        );
        assert_eq!(all_data_compact_block, reader_compact_block_all_data);

        println!("CompactBlock at height {} OK", height.0);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn get_compact_block_stream() {
    use futures::StreamExt;

    init_tracing();

    let (TestVectorData { blocks, .. }, _db_dir, _zaino_db, db_reader) =
        load_vectors_v1db_and_reader().await;

    let start_height = Height(blocks.first().unwrap().height);
    let end_height = Height(blocks.last().unwrap().height);

    for pool_type_filter in [PoolTypeFilter::default(), PoolTypeFilter::includes_all()] {
        let compact_block_stream = db_reader
            .get_compact_block_stream(start_height, end_height, pool_type_filter.clone())
            .await
            .unwrap();

        futures::pin_mut!(compact_block_stream);

        let mut expected_next_height_u32: u32 = start_height.0;
        let mut streamed_block_count: usize = 0;

        while let Some(block_result) = compact_block_stream.next().await {
            let streamed_compact_block = block_result.unwrap();

            let streamed_height_u32: u32 = u32::try_from(streamed_compact_block.height).unwrap();

            assert_eq!(streamed_height_u32, expected_next_height_u32);

            let singular_compact_block = db_reader
                .get_compact_block(Height(streamed_height_u32), pool_type_filter.clone())
                .await
                .unwrap();

            assert_eq!(singular_compact_block, streamed_compact_block);

            expected_next_height_u32 = expected_next_height_u32.saturating_add(1);
            streamed_block_count = streamed_block_count.saturating_add(1);
        }

        let expected_block_count: usize = (end_height
            .0
            .saturating_sub(start_height.0)
            .saturating_add(1)) as usize;

        assert_eq!(streamed_block_count, expected_block_count);
        assert_eq!(expected_next_height_u32, end_height.0.saturating_add(1));
    }
}

#[cfg(feature = "transparent_address_history_experimental")]
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

    for chain_block in indexed_block_chain(&blocks) {
        let block_height = chain_block.context.index.height;
        println!("Checking faucet txids at height {}", block_height.0);
        let block_txids: Vec<String> = chain_block
            .transactions()
            .iter()
            .map(|tx_data| tx_data.txid().encode_hex::<String>())
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
            reader_block_txids.push(txid.encode_hex::<String>());
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
        reader_faucet_txids.push(txid.encode_hex::<String>());
    }

    assert_eq!(faucet.txids.len(), reader_faucet_txids.len());
    assert_eq!(faucet.txids, reader_faucet_txids);
}

#[cfg(feature = "transparent_address_history_experimental")]
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

    for chain_block in indexed_block_chain(&blocks) {
        let block_height = chain_block.context.index.height;
        println!("Checking recipient txids at height {}", block_height.0);
        let block_txids: Vec<String> = chain_block
            .transactions()
            .iter()
            .map(|tx_data| tx_data.txid().encode_hex::<String>())
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
            reader_block_txids.push(txid.encode_hex::<String>());
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
        reader_recipient_txids.push(txid.encode_hex::<String>());
    }

    assert_eq!(recipient.txids.len(), reader_recipient_txids.len());
    assert_eq!(recipient.txids, reader_recipient_txids);
}

#[cfg(feature = "transparent_address_history_experimental")]
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
        cleaned_utxos.push((txid.encode_hex::<String>(), output_index.index(), satoshis));
    }

    let reader_faucet_utxo_indexes = db_reader
        .addr_utxos_by_range(faucet_addr_script, start, end)
        .await
        .unwrap()
        .unwrap();

    let mut reader_faucet_utxos = Vec::new();

    for (tx_location, vout, value) in reader_faucet_utxo_indexes {
        let txid = db_reader
            .get_txid(tx_location)
            .await
            .unwrap()
            .encode_hex::<String>();
        reader_faucet_utxos.push((txid, vout as u32, value));
    }

    assert_eq!(cleaned_utxos.len(), reader_faucet_utxos.len());
    assert_eq!(cleaned_utxos, reader_faucet_utxos);
}

#[cfg(feature = "transparent_address_history_experimental")]
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
        cleaned_utxos.push((txid.encode_hex::<String>(), output_index.index(), satoshis));
    }

    let reader_recipient_utxo_indexes = db_reader
        .addr_utxos_by_range(recipient_addr_script, start, end)
        .await
        .unwrap()
        .unwrap();

    let mut reader_recipient_utxos = Vec::new();

    for (tx_location, vout, value) in reader_recipient_utxo_indexes {
        let txid = db_reader
            .get_txid(tx_location)
            .await
            .unwrap()
            .encode_hex::<String>();
        reader_recipient_utxos.push((txid, vout as u32, value));
    }

    assert_eq!(cleaned_utxos.len(), reader_recipient_utxos.len());
    assert_eq!(cleaned_utxos, reader_recipient_utxos);
}

#[cfg(feature = "transparent_address_history_experimental")]
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

    let (indexed_blocks, tx_by_index) = index_test_vector_blocks(&blocks);

    let mut faucet_outpoints = Vec::new();
    let mut faucet_ouptpoints_spent_status = Vec::new();
    for chain_block in &indexed_blocks {
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
        faucet_utxo_indexes.push((txid.encode_hex::<String>(), output_index.index()));
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
            TransactionHash::from(*outpoint.prev_txid()).encode_hex::<String>(),
            outpoint.prev_index(),
        );
        match spender_option {
            Some(spender_index) => {
                let spender_tx = tx_by_index.get(&(
                    spender_index.block_height(),
                    spender_index.tx_index() as u64,
                ));
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

    let (indexed_blocks, tx_by_index) = index_test_vector_blocks(&blocks);

    let mut recipient_outpoints = Vec::new();
    let mut recipient_ouptpoints_spent_status = Vec::new();
    for chain_block in &indexed_blocks {
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
        recipient_utxo_indexes.push((txid.encode_hex::<String>(), output_index.index()));
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
            TransactionHash::from(*outpoint.prev_txid()).encode_hex::<String>(),
            outpoint.prev_index(),
        );
        match spender_option {
            Some(spender_index) => {
                let spender_tx = tx_by_index.get(&(
                    spender_index.block_height(),
                    spender_index.tx_index() as u64,
                ));
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

#[tokio::test(flavor = "multi_thread")]
async fn tx_out_set_info_accumulator_updates_on_write() {
    init_tracing();

    // Load the regtest vectors, write every vector block into FinalisedState, and wait until the
    // finalised state has finished its startup/background validation.
    let (TestVectorData { blocks, .. }, _db_dir, zaino_db) =
        load_vectors_and_spawn_and_sync_v1_zaino_db().await;

    zaino_db.wait_until_ready().await;

    let db_reader = Arc::new(zaino_db).to_reader();

    // Build the expected UTXO set directly from the same vector blocks.
    //
    // Map shape:
    //   txid -> { output_index -> TxOutCompact }
    //
    // From this we derive every accumulator field:
    //   transactions         = number of txids with at least one unspent output
    //   transaction_outputs  = total number of unspent transparent outputs
    //   bytes_serialized     = transaction_outputs * ZAINO_TXOUTSET_ENTRY_LEN
    //   hash_serialized      = XOR of tx_out_set_entry_digest over all unspent outputs
    //   total_zatoshis       = sum of `value` over all unspent outputs
    let mut unspent_output_indices_by_transaction_hash: HashMap<
        TransactionHash,
        HashMap<u32, crate::TxOutCompact>,
    > = HashMap::new();

    for chain_block in indexed_block_chain(&blocks) {
        for transaction in chain_block.transactions() {
            // First apply spends, removing spent transparent outputs from the expected UTXO set.
            for input in transaction.transparent().inputs() {
                if input.is_null_prevout() {
                    continue;
                }

                let previous_transaction_hash = TransactionHash::from(*input.prevout_txid());

                let unspent_output_indices = unspent_output_indices_by_transaction_hash
                    .get_mut(&previous_transaction_hash)
                    .unwrap_or_else(|| {
                        panic!(
                            "test vectors spend unknown transaction {previous_transaction_hash:?}"
                        )
                    });

                assert!(
                    unspent_output_indices
                        .remove(&input.prevout_index())
                        .is_some(),
                    "test vectors spend unknown output: transaction {:?}, output {}",
                    previous_transaction_hash,
                    input.prevout_index()
                );

                // If a transaction has no remaining unspent outputs, it should no longer
                // contribute to the accumulator's `transactions` count.
                if unspent_output_indices.is_empty() {
                    unspent_output_indices_by_transaction_hash.remove(&previous_transaction_hash);
                }
            }

            // Then apply outputs, adding newly-created transparent outputs to the expected UTXO set.
            if transaction.transparent().outputs().is_empty() {
                continue;
            }

            let transaction_hash = *transaction.txid();

            let unspent_output_indices = unspent_output_indices_by_transaction_hash
                .entry(transaction_hash)
                .or_default();

            for (output_index, output) in transaction.transparent().outputs().iter().enumerate() {
                // The accumulator skips NonStandard (unspendable) outputs — see
                // `is_unspendable_tx_out` in
                // `chain_index::types::db::metadata`. The oracle must mirror that.
                if crate::chain_index::types::db::metadata::is_unspendable_tx_out(output) {
                    continue;
                }

                let output_index = u32::try_from(output_index).unwrap();

                assert!(
                    unspent_output_indices
                        .insert(output_index, *output)
                        .is_none(),
                    "test vectors duplicate output index: transaction {transaction_hash:?}, output {output_index}"
                );
            }

            // If the transaction had only NonStandard outputs, drop the empty entry so it
            // doesn't inflate the expected `transactions` count.
            if unspent_output_indices.is_empty() {
                unspent_output_indices_by_transaction_hash.remove(&transaction_hash);
            }
        }
    }

    let expected_accumulator =
        accumulator_from_unspent_map(&unspent_output_indices_by_transaction_hash);

    // Check that the accumulator maintained by write_block matches the independently
    // reconstructed expected UTXO-set counts.
    let actual_accumulator = db_reader.get_tx_out_set_info_accumulator().await.unwrap();

    assert_eq!(expected_accumulator, actual_accumulator);
}

/// The bulk sequential accumulator builder must produce exactly the accumulator that the
/// per-block incremental write path maintained, for every shard count. Sharding partitions the
/// work by creating-txid prefix and recombines the partials; the result must be shard-count
/// independent.
#[tokio::test(flavor = "multi_thread")]
async fn bulk_tx_out_set_accumulator_builder_matches_incremental() {
    init_tracing();

    let (_data, _db_dir, zaino_db) = load_vectors_and_spawn_and_sync_v1_zaino_db().await;
    zaino_db.wait_until_ready().await;

    use crate::chain_index::finalised_state::capability::{
        CapabilityRequest, DbRead, TransparentHistExt,
    };

    let backend = zaino_db
        .backend_for_cap(CapabilityRequest::WriteCore)
        .unwrap();

    let db_tip = backend.db_height().await.unwrap().unwrap();
    let incremental = backend.get_tx_out_set_info_accumulator().await.unwrap();

    // 1 = single optimal pass; >1 exercises the sharded multi-pass recombination; 256 = one
    // first-byte value per shard (maximal sharding).
    for shards in [1u16, 2, 4, 256] {
        let built = tokio::task::block_in_place(|| {
            backend.build_tx_out_set_accumulator_blocking(db_tip, shards)
        })
        .unwrap();

        assert_eq!(
            built, incremental,
            "bulk builder (shards={shards}) must equal the incrementally-maintained accumulator"
        );
    }
}

/// The write path must advance the validated tip itself (via the cheap in-memory parent + merkle
/// checks), so reads never fall back to the expensive read-back validation. This must hold right
/// after a sync completes, independent of the background validator.
#[tokio::test(flavor = "multi_thread")]
async fn write_path_advances_validated_tip() {
    init_tracing();

    let (_data, _db_dir, zaino_db) = load_vectors_and_spawn_and_sync_v1_zaino_db().await;

    // Intentionally do NOT call `wait_until_ready` (which would let the background validator run):
    // the bulk write path should have marked every synced height validated by the time
    // `sync_to_height` returned.
    let backend = zaino_db
        .backend_for_cap(
            crate::chain_index::finalised_state::capability::CapabilityRequest::WriteCore,
        )
        .unwrap();

    use crate::chain_index::finalised_state::capability::DbRead;
    let db_tip = backend.db_height().await.unwrap().unwrap();

    assert_eq!(
        backend.validated_tip_height(),
        db_tip.0,
        "write path must advance validated_tip to the synced tip"
    );
}

/// Syncs the vector chain to height 200 with the given bulk-write batch budget and returns the
/// resulting `(db tip, validated tip, txout-set accumulator)`.
async fn sync_with_batch_budget(
    blocks: Vec<TestVectorBlockData>,
    sync_write_batch_bytes: u64,
) -> (Height, u32, FinalisedTxOutSetInfoAccumulator) {
    use crate::chain_index::finalised_state::capability::{
        CapabilityRequest, DbRead, TransparentHistExt,
    };

    let source = build_mockchain_source(blocks);
    let temp_dir: TempDir = tempfile::tempdir().unwrap();
    let config = ChainIndexConfig {
        storage: StorageConfig {
            database: DatabaseConfig {
                path: temp_dir.path().to_path_buf(),
                sync_write_batch_bytes,
                ..Default::default()
            },
            ..Default::default()
        },
        ephemeral: false,
        db_version: 1,
        network: Network::Regtest(ActivationHeights::default()),
    };

    let zaino_db = FinalisedState::spawn(config, source.clone()).await.unwrap();
    zaino_db.sync_to_height(Height(200), &source).await.unwrap();
    // Catch-up of >LONG_RUNNING_SYNC_THRESHOLD blocks runs in the background; wait for the
    // persistent DB to actually reach the tip before reading it back.
    zaino_db.wait_until_synced().await;

    let backend = zaino_db
        .backend_for_cap(CapabilityRequest::WriteCore)
        .unwrap();
    let db_tip = backend.db_height().await.unwrap().unwrap();
    let validated_tip = backend.validated_tip_height();
    let accumulator = backend.get_tx_out_set_info_accumulator().await.unwrap();

    (db_tip, validated_tip, accumulator)
}

/// The bulk-sync result must be independent of the write-batch budget: a single huge batch and a
/// one-block-per-batch sync of the same chain must produce an identical db tip, validated tip, and
/// txout-set accumulator. This exercises the cross-batch continuity chaining, per-batch
/// `validated_tip` advance, and sorted-insert flush boundaries that a single-batch sync does not.
#[tokio::test(flavor = "multi_thread")]
async fn batched_sync_is_batch_size_independent() {
    init_tracing();

    let blocks = load_test_vectors().unwrap().blocks;

    // u64::MAX => the whole sync is one batch; 1 => every block exceeds the budget => one block per
    // batch (a flush + commit + fsync after each block).
    let single_batch = sync_with_batch_budget(blocks.clone(), u64::MAX).await;
    let per_block_batches = sync_with_batch_budget(blocks, 1).await;

    assert_eq!(single_batch.0, per_block_batches.0, "db tip must match");
    assert_eq!(
        single_batch.1, per_block_batches.1,
        "validated tip must match"
    );
    assert_eq!(
        single_batch.2, per_block_batches.2,
        "txout-set accumulator must be independent of the write-batch budget"
    );
}

/// The incremental range-update path — taken when a catch-up advances an already-built accumulator
/// by a small range (`write_blocks_to_height`'s steady-state branch) — must produce exactly the
/// accumulator a full from-genesis rebuild produces at the same tip, for all five fields. This is
/// the correctness gate for `update_tx_out_set_accumulator_for_range`: with regtest coinbase
/// maturity of 100, splitting the sync at height 100 guarantees the second segment spends outputs
/// created in the first (exercising the `transactions` "Set B" decrement) as well as outputs both
/// created and spent within the range (the XOR-cancel case).
#[tokio::test(flavor = "multi_thread")]
async fn incremental_accumulator_update_matches_full_rebuild() {
    init_tracing();

    use crate::chain_index::finalised_state::capability::{
        CapabilityRequest, DbRead, TransparentHistExt,
    };

    let blocks = load_test_vectors().unwrap().blocks;
    let source = build_mockchain_source(blocks);
    let temp_dir: TempDir = tempfile::tempdir().unwrap();
    let config = ChainIndexConfig {
        storage: StorageConfig {
            database: DatabaseConfig {
                path: temp_dir.path().to_path_buf(),
                ..Default::default()
            },
            ..Default::default()
        },
        ephemeral: false,
        db_version: 1,
        network: Network::Regtest(ActivationHeights::default()),
    };

    let zaino_db = FinalisedState::spawn(config, source.clone()).await.unwrap();

    // First segment builds the accumulator to height 100 (no watermark yet => full rebuild),
    // the second advances it by a 100-block range => the incremental update path under test.
    zaino_db.sync_to_height(Height(100), &source).await.unwrap();
    // Background catch-up (>LONG_RUNNING_SYNC_THRESHOLD); wait for the persistent build + watermark.
    zaino_db.wait_until_synced().await;

    let backend = zaino_db
        .backend_for_cap(CapabilityRequest::WriteCore)
        .unwrap();

    // The watermark must sit at 100 here: that (together with gap 100 <= the incremental cap)
    // pins the next sync to the incremental branch rather than a silent rebuild fallback that
    // would make the comparison below trivial.
    assert_eq!(
        backend
            .read_tx_out_set_accumulator_built_height()
            .await
            .unwrap(),
        Some(Height(100)),
        "first segment must leave the accumulator watermark at the synced tip"
    );

    zaino_db.sync_to_height(Height(200), &source).await.unwrap();
    // Background catch-up; wait for the incremental accumulator update to advance the watermark.
    zaino_db.wait_until_synced().await;

    let db_tip = backend.db_height().await.unwrap().unwrap();
    assert_eq!(db_tip, Height(200), "both segments must have been synced");
    assert_eq!(
        backend
            .read_tx_out_set_accumulator_built_height()
            .await
            .unwrap(),
        Some(Height(200)),
        "incremental update must advance the watermark to the new tip"
    );

    let incremental = backend.get_tx_out_set_info_accumulator().await.unwrap();
    let from_genesis =
        tokio::task::block_in_place(|| backend.build_tx_out_set_accumulator_blocking(db_tip, 1))
            .unwrap();

    assert_eq!(
        incremental, from_genesis,
        "incremental range-update accumulator must equal the from-genesis rebuild at the tip"
    );
}

/// Computes the canonical [`FinalisedTxOutSetInfoAccumulator`] for a fully-resolved UTXO set,
/// used as the source of truth by the write/delete accumulator tests.
fn accumulator_from_unspent_map(
    unspent: &HashMap<TransactionHash, HashMap<u32, crate::TxOutCompact>>,
) -> FinalisedTxOutSetInfoAccumulator {
    use crate::chain_index::types::db::metadata::{
        tx_out_set_entry_digest, ZAINO_TXOUTSET_ENTRY_LEN,
    };
    use crate::Outpoint;

    let mut transaction_outputs = 0u64;
    let mut total_zatoshis = 0u64;
    let mut hash_serialized = [0u8; 32];

    for (txid, outputs) in unspent {
        for (output_index, out) in outputs {
            let outpoint = Outpoint::new(txid.0, *output_index);
            let digest = tx_out_set_entry_digest(&outpoint, out);
            for (dst, src) in hash_serialized.iter_mut().zip(digest.iter()) {
                *dst ^= *src;
            }
            transaction_outputs += 1;
            total_zatoshis += out.value();
        }
    }

    FinalisedTxOutSetInfoAccumulator {
        transactions: unspent.len() as u64,
        transaction_outputs,
        bytes_serialized: transaction_outputs * ZAINO_TXOUTSET_ENTRY_LEN,
        hash_serialized,
        total_zatoshis,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn tx_out_set_info_accumulator_updates_on_delete() {
    init_tracing();

    // Load and write all vector blocks, then delete the current tip block.
    // The accumulator should end up matching the UTXO set for all blocks except the deleted tip.
    let (TestVectorData { blocks, .. }, _db_dir, zaino_db) =
        load_vectors_and_spawn_and_sync_v1_zaino_db().await;

    zaino_db.wait_until_ready().await;

    let deleted_block_height = Height(blocks.last().unwrap().height);

    zaino_db
        .delete_block_at_height(deleted_block_height)
        .await
        .unwrap();

    zaino_db.wait_until_ready().await;

    let db_reader = Arc::new(zaino_db).to_reader();

    // Rebuild the expected UTXO set from the vector chain with the deleted tip excluded.
    //
    // This verifies that delete_block reverses every accumulator field:
    //   transactions, transaction_outputs, bytes_serialized, hash_serialized, total_zatoshis.
    let mut unspent_output_indices_by_transaction_hash: HashMap<
        TransactionHash,
        HashMap<u32, crate::TxOutCompact>,
    > = HashMap::new();

    for chain_block in indexed_block_chain(&blocks[..blocks.len() - 1]) {
        for transaction in chain_block.transactions() {
            // Remove any transparent outputs spent by this transaction.
            for input in transaction.transparent().inputs() {
                if input.is_null_prevout() {
                    continue;
                }

                let previous_transaction_hash = TransactionHash::from(*input.prevout_txid());

                let unspent_output_indices = unspent_output_indices_by_transaction_hash
                    .get_mut(&previous_transaction_hash)
                    .unwrap_or_else(|| {
                        panic!(
                            "test vectors spend unknown transaction {previous_transaction_hash:?}"
                        )
                    });

                assert!(
                    unspent_output_indices
                        .remove(&input.prevout_index())
                        .is_some(),
                    "test vectors spend unknown output: transaction {:?}, output {}",
                    previous_transaction_hash,
                    input.prevout_index()
                );

                if unspent_output_indices.is_empty() {
                    unspent_output_indices_by_transaction_hash.remove(&previous_transaction_hash);
                }
            }

            // Add this transaction's newly-created transparent outputs.
            if transaction.transparent().outputs().is_empty() {
                continue;
            }

            let transaction_hash = *transaction.txid();

            let unspent_output_indices = unspent_output_indices_by_transaction_hash
                .entry(transaction_hash)
                .or_default();

            for (output_index, output) in transaction.transparent().outputs().iter().enumerate() {
                // The accumulator skips NonStandard (unspendable) outputs — see
                // `is_unspendable_tx_out` in
                // `chain_index::types::db::metadata`. The oracle must mirror that.
                if crate::chain_index::types::db::metadata::is_unspendable_tx_out(output) {
                    continue;
                }

                let output_index = u32::try_from(output_index).unwrap();

                assert!(
                    unspent_output_indices
                        .insert(output_index, *output)
                        .is_none(),
                    "test vectors duplicate output index: transaction {transaction_hash:?}, output {output_index}"
                );
            }

            // If the transaction had only NonStandard outputs, drop the empty entry so it
            // doesn't inflate the expected `transactions` count.
            if unspent_output_indices.is_empty() {
                unspent_output_indices_by_transaction_hash.remove(&transaction_hash);
            }
        }
    }

    let expected_accumulator =
        accumulator_from_unspent_map(&unspent_output_indices_by_transaction_hash);

    // Check the accumulator persisted by delete_block_at_height/delete_block.
    let actual_accumulator = db_reader.get_tx_out_set_info_accumulator().await.unwrap();

    assert_eq!(expected_accumulator, actual_accumulator);
}
