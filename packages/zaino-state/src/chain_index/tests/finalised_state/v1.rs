//! Holds tests for the V1 database.

use hex::ToHex;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;
use zaino_common::network::ActivationHeights;
use zaino_common::{DatabaseConfig, Network, StorageConfig};
use zaino_proto::proto::utils::{compact_block_with_pool_types, PoolTypeFilter};

use crate::chain_index::finalised_state::capability::{
    BlockCoreExt as _, DbRead as _, DbWrite as _, IndexedBlockExt, TransparentHistExt as _,
};
use crate::chain_index::finalised_state::db::DbBackend;
use crate::chain_index::finalised_state::reader::DbReader;
use crate::chain_index::finalised_state::write_batch::WriteBatcher;
use crate::chain_index::finalised_state::ZainoDB;
use crate::chain_index::source::mockchain_source::MockchainSource;
use crate::chain_index::tests::init_tracing;
use crate::chain_index::tests::vectors::{
    build_mockchain_source, copy_dir_recursive, index_test_vector_blocks, indexed_block_chain,
    load_test_vectors, sync_db_with_blockdata, test_vector_block_metadata, TestVectorBlockData,
    TestVectorData,
};

use crate::chain_index::types::TransactionHash;

use crate::chain_index::types::db::metadata::FinalisedTxOutSetInfoAccumulator;
use crate::chain_index::types::Height;
use crate::error::FinalisedStateError;
use crate::{
    BlockCacheConfig, BlockMetadata, BlockWithMetadata, ChainWork, IndexedBlock, TxLocation,
};

use crate::{AddrScript, Outpoint};

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

    zaino_db.wait_until_ready().await;
    dbg!(zaino_db.status());
    let built_db_height = dbg!(zaino_db.db_height().await.unwrap()).unwrap();

    assert_eq!(built_db_height, Height(200));
}

/// Regression test: a `sync_to_height` call that *resumes* a non-empty database
/// preserves cumulative chainwork.
///
/// On resume, `sync_to_height` seeds `parent_chainwork` from the stored tip's
/// own header. (It previously read the *target* height's header, which isn't in
/// the database yet, so the seed silently fell to zero and every block written
/// on resume lost all the work accumulated before the resume point.)
///
/// The test syncs the same chain two ways — one continuous pass versus a split
/// (sync to SPLIT, then resume to TARGET) — and asserts the first block the
/// resuming call writes has the same stored chainwork either way.
#[tokio::test(flavor = "multi_thread")]
async fn resume_sync_preserves_cumulative_chainwork() {
    init_tracing();

    const SPLIT: u32 = 100;
    const TARGET: u32 = 200;
    const PROBE: u32 = SPLIT + 1; // first block written by the resuming call

    let blocks = load_test_vectors().unwrap().blocks;

    // Reference: one continuous sync covering 0..=TARGET.
    let reference_source = build_mockchain_source(blocks.clone());
    let (_reference_dir, reference_db) = spawn_v1_zaino_db(reference_source.clone()).await.unwrap();
    reference_db
        .sync_to_height(crate::chain_index::types::Height(TARGET), &reference_source)
        .await
        .unwrap();
    reference_db.wait_until_ready().await;
    let reference_chainwork = Arc::new(reference_db)
        .to_reader()
        .get_block_header(crate::chain_index::types::Height(PROBE))
        .await
        .unwrap()
        .context
        .chainwork;

    // Resume: sync 0..=SPLIT, then resume SPLIT+1..=TARGET in a second call.
    let resume_source = build_mockchain_source(blocks);
    let (_resume_dir, resume_db) = spawn_v1_zaino_db(resume_source.clone()).await.unwrap();
    resume_db
        .sync_to_height(crate::chain_index::types::Height(SPLIT), &resume_source)
        .await
        .unwrap();
    resume_db.wait_until_ready().await;
    resume_db
        .sync_to_height(crate::chain_index::types::Height(TARGET), &resume_source)
        .await
        .unwrap();
    resume_db.wait_until_ready().await;
    let resume_chainwork = Arc::new(resume_db)
        .to_reader()
        .get_block_header(crate::chain_index::types::Height(PROBE))
        .await
        .unwrap()
        .context
        .chainwork;

    assert_eq!(
        resume_chainwork, reference_chainwork,
        "resume chainwork mismatch at height {PROBE}: \
         resume={resume_chainwork:?} reference={reference_chainwork:?}"
    );
}

/// Regression test: when building an `IndexedBlock` fails mid-sync,
/// `sync_to_height` names the height of the block that actually failed (the
/// loop's `height_int`), not the sync target. (It previously interpolated the
/// target `height.0`, reporting a failure at height 1 as "height 5".)
///
/// The test serves a fetchable-but-unbuildable block at height 1 (its
/// transactions cleared, so `coinbase_height()` is `None` and
/// `IndexedBlock::try_from` fails) while targeting height 5.
#[tokio::test(flavor = "multi_thread")]
async fn build_failure_names_failing_height_not_target() {
    init_tracing();

    const FAIL_HEIGHT: usize = 1;
    const TARGET: u32 = 5;

    let mut blocks = load_test_vectors().unwrap().blocks;
    blocks.truncate((TARGET as usize) + 1); // heights 0..=TARGET, tip stays valid

    // Clear the transactions of the block at FAIL_HEIGHT: it still fetches (the
    // header, and thus the hash, is unchanged) but `coinbase_height()` becomes
    // `None`, so `IndexedBlock::try_from` fails when sync reaches it.
    blocks[FAIL_HEIGHT].zebra_block.transactions.clear();

    let source = build_mockchain_source(blocks);
    let (_db_dir, zaino_db) = spawn_v1_zaino_db(source.clone()).await.unwrap();

    let err = zaino_db
        .sync_to_height(crate::chain_index::types::Height(TARGET), &source)
        .await
        .expect_err("building the headless block at height 1 should fail the sync");
    let msg = err.to_string();

    assert!(
        msg.contains("error building block data"),
        "expected the IndexedBlock build-failure path; got: {msg}"
    );
    assert!(
        msg.contains(&format!("height {FAIL_HEIGHT}")),
        "build error should name the failing height ({FAIL_HEIGHT}), not the target; got: {msg}"
    );
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
            .delete_block_at_height(crate::chain_index::types::Height(h))
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

    let fixture_db_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("chain_index")
        .join("tests")
        .join("vectors")
        .join("v1_test_db");
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("v1_test_db");
    copy_dir_recursive(&fixture_db_path, &db_path).unwrap();

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

    chain_block.context.index.height = crate::chain_index::types::Height(height + 1);
    dbg!(chain_block.context.index.height);

    let db_err = dbg!(zaino_db.write_block(chain_block).await);

    // TODO: Update with concrete err type.
    assert!(db_err.is_err());

    dbg!(zaino_db.db_height().await.unwrap());
}

/// Spawns a v1 `DbBackend` over a fresh tempdir, for tests that need direct
/// backend access (e.g. the batched write path and accumulator reads).
async fn spawn_fresh_v1_backend() -> (TempDir, DbBackend) {
    let temp_dir = tempfile::tempdir().expect("create tempdir");
    let config = BlockCacheConfig {
        storage: StorageConfig {
            database: DatabaseConfig {
                path: temp_dir.path().to_path_buf(),
                ..Default::default()
            },
            ..Default::default()
        },
        db_version: 1,
        network: Network::Regtest(ActivationHeights::default()),
    };
    let backend = DbBackend::spawn_v1(&config)
        .await
        .expect("spawn v1 backend");
    (temp_dir, backend)
}

/// Batched ingest must be indistinguishable from per-block ingest: same tip,
/// same `txid_location` mappings, and the same txout-set accumulator (the
/// batched path threads the accumulator in memory through each batch instead
/// of re-reading committed state).
#[tokio::test(flavor = "multi_thread")]
async fn write_blocks_batched_ingest_matches_per_block() {
    let TestVectorData { blocks, .. } = load_test_vectors().expect("test vectors load");
    let chain: Vec<IndexedBlock> = indexed_block_chain(&blocks).collect();

    let (_dir_reference, reference_backend) = spawn_fresh_v1_backend().await;
    for block in &chain {
        reference_backend
            .write_block(block.clone())
            .await
            .expect("per-block write");
    }

    let (_dir_batched, batched_backend) = spawn_fresh_v1_backend().await;
    // A small budget forces many batches (and dependency splits where the
    // vector chain spends recently created outputs) instead of one big batch.
    let mut batcher = WriteBatcher::new(64 * 1024);
    let mut batch_count = 0;
    for block in chain.iter().cloned() {
        if let Some(batch) = batcher.push(block) {
            batched_backend
                .write_blocks(&batch)
                .await
                .expect("batched write");
            batch_count += 1;
        }
    }
    if let Some(batch) = batcher.flush() {
        batched_backend
            .write_blocks(&batch)
            .await
            .expect("final batched write");
        batch_count += 1;
    }
    assert!(
        batch_count > 1,
        "small budget should split the chain into multiple batches"
    );

    assert_eq!(
        reference_backend
            .db_height()
            .await
            .expect("reference db_height"),
        batched_backend
            .db_height()
            .await
            .expect("batched db_height"),
    );

    for block in &chain {
        let height = block.context.index.height;
        for tx in block.transactions() {
            let reference_location = reference_backend
                .get_tx_location(tx.txid())
                .await
                .expect("reference get_tx_location");
            let batched_location = batched_backend
                .get_tx_location(tx.txid())
                .await
                .expect("batched get_tx_location");
            assert_eq!(
                reference_location,
                batched_location,
                "txid {:?} location diverges between per-block and batched ingest \
                 (height {})",
                tx.txid(),
                height.0,
            );
        }
    }

    assert_eq!(
        reference_backend
            .get_tx_out_set_info_accumulator()
            .await
            .expect("reference accumulator"),
        batched_backend
            .get_tx_out_set_info_accumulator()
            .await
            .expect("batched accumulator"),
        "batched accumulator threading must reproduce the per-block accumulator",
    );
}

/// The batcher must partition the chain on its byte budget alone: chunks
/// concatenate back to the original order, a tiny budget produces many
/// chunks, and a budget larger than the whole chain produces exactly one.
/// (Intra-batch transparent dependencies are permitted — the batched write
/// path resolves them through its `PendingBatchState` overlay.)
#[test]
fn write_batcher_partitions_chain_on_byte_budget() {
    let TestVectorData { blocks, .. } = load_test_vectors().expect("test vectors load");
    let chain: Vec<IndexedBlock> = indexed_block_chain(&blocks).collect();

    let mut batcher = WriteBatcher::new(16 * 1024);
    let mut chunks: Vec<Vec<IndexedBlock>> = Vec::new();
    for block in chain.iter().cloned() {
        if let Some(batch) = batcher.push(block) {
            chunks.push(batch);
        }
    }
    if let Some(batch) = batcher.flush() {
        chunks.push(batch);
    }

    let flattened: Vec<u32> = chunks
        .iter()
        .flatten()
        .map(|block| block.context.index.height.0)
        .collect();
    let expected: Vec<u32> = chain
        .iter()
        .map(|block| block.context.index.height.0)
        .collect();
    assert_eq!(
        flattened, expected,
        "chunks must partition the chain in order"
    );
    assert!(chunks.len() > 1, "tiny budget must produce multiple chunks");

    // A budget far above the whole chain's write volume yields a single batch.
    let mut unbounded = WriteBatcher::new(usize::MAX);
    for block in chain.iter().cloned() {
        assert!(
            unbounded.push(block).is_none(),
            "an unbounded budget must never flush mid-chain"
        );
    }
    let single = unbounded.flush().expect("pending blocks must flush");
    assert_eq!(single.len(), chain.len());
}

/// `write_blocks` is a strict batch primitive: gaps within the batch and
/// batches that do not start at `db_tip + 1` are rejected before anything is
/// written.
#[tokio::test(flavor = "multi_thread")]
async fn write_blocks_rejects_malformed_batches() {
    let TestVectorData { blocks, .. } = load_test_vectors().expect("test vectors load");
    let chain: Vec<IndexedBlock> = indexed_block_chain(&blocks).collect();
    let (_db_dir, backend) = spawn_fresh_v1_backend().await;

    let gapped = vec![chain[0].clone(), chain[2].clone()];
    let gap_error = backend
        .write_blocks(&gapped)
        .await
        .expect_err("gapped batch must be rejected");
    assert!(
        gap_error.to_string().contains("not height-contiguous"),
        "unexpected error for gapped batch: {gap_error}"
    );

    let offset = chain[1..3].to_vec();
    let offset_error = backend
        .write_blocks(&offset)
        .await
        .expect_err("batch not starting at genesis on an empty DB must be rejected");
    assert!(
        offset_error.to_string().contains("empty database"),
        "unexpected error for offset batch: {offset_error}"
    );

    // The rejected batches must not have written anything.
    assert_eq!(backend.db_height().await.expect("db_height"), None);

    backend
        .write_blocks(&chain[..3])
        .await
        .expect("valid batch write");
    assert_eq!(
        backend.db_height().await.expect("db_height"),
        Some(Height(2))
    );
}

/// The unspent-output count cache is an optimization, never a semantic input:
/// deleting blocks (which reverses cached counts) and re-writing them must
/// reproduce the exact accumulator.
#[tokio::test(flavor = "multi_thread")]
async fn accumulator_roundtrips_through_delete_and_rewrite() {
    let TestVectorData { blocks, .. } = load_test_vectors().expect("test vectors load");
    let chain: Vec<IndexedBlock> = indexed_block_chain(&blocks).collect();
    let (_db_dir, backend) = spawn_fresh_v1_backend().await;

    for block in &chain {
        backend.write_block(block.clone()).await.expect("write");
    }
    let full_accumulator = backend
        .get_tx_out_set_info_accumulator()
        .await
        .expect("accumulator after full ingest");

    let tip = chain.len() as u32 - 1;
    let cut = tip - 50;
    for height in ((cut + 1)..=tip).rev() {
        backend
            .delete_block_at_height(Height(height))
            .await
            .expect("delete tip block");
    }
    for block in &chain[(cut as usize + 1)..] {
        backend.write_block(block.clone()).await.expect("re-write");
    }

    assert_eq!(
        backend
            .get_tx_out_set_info_accumulator()
            .await
            .expect("accumulator after round-trip"),
        full_accumulator,
    );
}

/// Forces the cold-cache (probe) path mid-ingest and requires the same
/// accumulator as an uninterrupted warm-cache ingest — the cache must be pure
/// optimization, with the probe fallback semantically identical.
#[tokio::test(flavor = "multi_thread")]
async fn accumulator_is_identical_with_cold_unspent_count_cache() {
    let TestVectorData { blocks, .. } = load_test_vectors().expect("test vectors load");
    let chain: Vec<IndexedBlock> = indexed_block_chain(&blocks).collect();

    let (_dir_warm, warm_backend) = spawn_fresh_v1_backend().await;
    let (_dir_cold, cold_backend) = spawn_fresh_v1_backend().await;

    for (index, block) in chain.iter().enumerate() {
        warm_backend
            .write_block(block.clone())
            .await
            .expect("warm write");
        cold_backend
            .write_block(block.clone())
            .await
            .expect("cold write");
        if index % 32 == 0 {
            let DbBackend::V1(db) = &cold_backend else {
                panic!("spawn_fresh_v1_backend returned a non-V1 backend");
            };
            db.invalidate_unspent_output_counts();
        }
    }

    assert_eq!(
        warm_backend
            .get_tx_out_set_info_accumulator()
            .await
            .expect("warm accumulator"),
        cold_backend
            .get_tx_out_set_info_accumulator()
            .await
            .expect("cold accumulator"),
    );
}

/// Regression test for the `write_block` indexing loop: a fresh ingest must
/// leave every transaction resolvable through the `txid_location` reverse
/// index, with the location matching the transaction's position in the raw
/// vector chain. (Migration tests cover rebuilding this index; this covers
/// populating it on first write.)
#[tokio::test(flavor = "multi_thread")]
async fn write_block_populates_txid_location_index() {
    let (TestVectorData { blocks, .. }, _db_dir, _zaino_db, db_reader) =
        load_vectors_v1db_and_reader().await;

    for vector in &blocks {
        for (tx_index, transaction) in vector.zebra_block.transactions.iter().enumerate() {
            let txid = TransactionHash::from(transaction.hash());
            let tx_index = u16::try_from(tx_index).expect("vector block tx count fits in u16");
            let expected_location = TxLocation::new(vector.height, tx_index);

            let found_location = db_reader
                .get_tx_location(&txid)
                .await
                .expect("get_tx_location");

            assert_eq!(
                found_location,
                Some(expected_location),
                "txid {txid:?} (height {}, index {tx_index}) missing or mismatched in \
                 txid_location index",
                vector.height,
            );
        }
    }
}

/// Regression test for the duplicate-txid guard in `write_block`: a block
/// containing the same transaction twice must be rejected as invalid and must
/// not advance the DB tip.
#[tokio::test(flavor = "multi_thread")]
async fn write_block_rejects_duplicate_txid() {
    init_tracing();

    let test_vector_data = load_test_vectors().expect("test vectors load");
    let blocks = test_vector_data.blocks;
    let last_vector = blocks.last().expect("test vectors are non-empty").clone();

    let source = build_mockchain_source(blocks.clone());
    let (_db_dir, zaino_db) = spawn_v1_zaino_db(source)
        .await
        .expect("spawn ZainoDB for duplicate-txid test");
    // Sync everything below the last vector block so it is the next expected write.
    sync_db_with_blockdata(zaino_db.router(), blocks, Some(last_vector.height - 1)).await;

    let mut tampered_block = last_vector.zebra_block.clone();
    let duplicated_transaction = tampered_block
        .transactions
        .last()
        .expect("vector block has at least a coinbase transaction")
        .clone();
    tampered_block.transactions.push(duplicated_transaction);

    let metadata = test_vector_block_metadata(&last_vector, ChainWork::from_u256(0.into()));
    let chain_block = IndexedBlock::try_from(BlockWithMetadata::new(&tampered_block, metadata))
        .expect("tampered block converts to IndexedBlock");

    match zaino_db.write_block(chain_block).await {
        Err(FinalisedStateError::InvalidBlock { reason, .. }) => assert!(
            reason.contains("duplicate transaction hash"),
            "expected duplicate-txid rejection, got: {reason}"
        ),
        other => panic!("expected InvalidBlock for duplicate txid, got {other:?}"),
    }

    // The rejected block must not have advanced the tip.
    assert_eq!(
        zaino_db
            .db_height()
            .await
            .expect("db_height after rejected write"),
        Some(Height(last_vector.height - 1)),
    );
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
            .delete_block_at_height(crate::chain_index::types::Height(delete_height))
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

    // Load the regtest vectors, write every vector block into ZainoDB, and wait until the
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
