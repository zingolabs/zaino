//! Holds tests for the V1 database.

use hex::ToHex;
use std::path::PathBuf;
use tempfile::TempDir;
use zaino_common::network::ActivationHeights;
use zaino_common::{DatabaseConfig, StorageConfig};
use zaino_proto::proto::utils::{compact_block_with_pool_types, PoolTypeFilter};

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

use crate::chain_index::finalised_state::entry::StoredEntryVar;
use crate::error::FinalisedStateError;
use crate::{
    BlockHeaderData, BlockMetadata, BlockWithMetadata, ChainIndexConfig, Height, IndexedBlock,
    ZainoVersionedSerde as _,
};

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
        network: ActivationHeights::default().to_regtest_network(),
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

/// Regression test: blocks with no ironwood data must have NO ironwood row.
///
/// The ironwood table is sparse — readers treat an absent row as "no ironwood data"
/// (an absent row reads back as an empty list; a stored all-`None` list reads back
/// with one `None` per transaction). The write path instead wrote an all-`None`
/// `OrchardTxList` for every block, paying one serialization + LMDB put per block
/// across the entire pre-NU6.3 chain for zero information.
///
/// The test vectors are pre-NU6.3 regtest blocks, so no block carries ironwood data.
///
/// multi_thread required: DbV1 reads run under `tokio::task::block_in_place`.
#[tokio::test(flavor = "multi_thread")]
async fn no_ironwood_row_for_blocks_without_ironwood_data() {
    init_tracing();

    let (_test_vector_data, _db_dir, _zaino_db, db_reader) = load_vectors_v1db_and_reader().await;

    let ironwood_list = db_reader
        .get_block_ironwood(crate::Height(1))
        .await
        .unwrap();
    assert!(
        ironwood_list.tx().is_empty(),
        "no ironwood row may be written for a block without ironwood data \
         (read back {} entries; an absent row reads back as an empty list)",
        ironwood_list.tx().len(),
    );
}

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
        network: ActivationHeights::default().to_regtest_network(),
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
        network: ActivationHeights::default().to_regtest_network(),
    };
    let finalized_state_backend: FinalisedSource<MockchainSource> =
        FinalisedSource::spawn_v1(&config).await.unwrap();

    // Read block headers directly from the `headers` table rather than via `get_chain_block`, which
    // reconstructs the full block and validates (reading the v1.3.0 commitment table). This fixture
    // is a legacy database whose commitment rows are in `commitment_tree_data_1_0_0`, so validation
    // would fail; the header context asserted here is unaffected.
    let read_header_direct = |height: Height| -> Option<BlockHeaderData> {
        use lmdb::Transaction as _;
        let environment = finalized_state_backend.env().unwrap();
        let headers_database = environment.open_db(Some("headers_1_0_0")).unwrap();
        let transaction = environment.begin_ro_txn().unwrap();
        match transaction.get(headers_database, &height.to_bytes().unwrap()) {
            Ok(raw) => Some(
                *StoredEntryVar::<BlockHeaderData>::from_bytes(raw)
                    .unwrap()
                    .inner(),
            ),
            Err(lmdb::Error::NotFound) => None,
            Err(error) => panic!("failed to read header at height {}: {error}", height.0),
        }
    };

    let mut prev_hash = None;
    for height in 0..=100 {
        let header = read_header_direct(Height(height)).unwrap();
        if let Some(prev_hash) = prev_hash {
            assert_eq!(prev_hash, header.context.parent_hash);
        }
        prev_hash = Some(header.context.index.hash);
        assert_eq!(header.context.index.height, Height(height));
    }
    assert!(read_header_direct(Height(101)).is_none());
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

    let metadata = BlockMetadata::new(
        sapling_root,
        sapling_tree_size as u32,
        orchard_root,
        orchard_tree_size as u32,
        None,
        None, // no parent chainwork for this test
        ActivationHeights::default().to_regtest_network(),
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
