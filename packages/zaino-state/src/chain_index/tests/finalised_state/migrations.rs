//! Holds database migration tests.

use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;
use zaino_common::network::ActivationHeights;
use zaino_common::{DatabaseConfig, Network, StorageConfig};

use crate::chain_index::finalised_state::capability::{
    BlockCoreExt as _, DbCore as _, DbRead as _, DbVersion, DbWrite as _, MigrationStatus,
};
use crate::chain_index::finalised_state::db::v1::DB_SCHEMA_V1_HASH;
use crate::chain_index::finalised_state::db::DbBackend;
use crate::chain_index::finalised_state::entry::StoredEntryVar;
use crate::chain_index::finalised_state::ZainoDB;
use crate::chain_index::tests::init_tracing;
use crate::chain_index::tests::vectors::{
    build_mockchain_source, load_test_vectors, TestVectorData,
};
use crate::{
    version, BlockCacheConfig, BlockHeaderData, CompactSize, Height, ZainoVersionedSerde as _,
};

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

/// Tests that a database containing a mix of V1-encoded and V2-encoded
/// `BlockHeaderData` entries — which arises when a v1.0.0 deployment is upgraded
/// to v1.1.0 mid-chain — deserialises every header correctly and returns the right
/// height for every block.
///
/// Scenario:
///   Phase 1 – write first half of the chain using the normal V2 path, then
///              backpatch those header entries in LMDB to use the V1 wire format
///              (simulating a v1.0.0 on-disk state) and downgrade the stored
///              metadata to 1.0.0.
///   Phase 2 – reopen with ZainoDB (target 1.1.0); the metadata-only migration
///              runs, bumping the version without touching the headers table.
///   Phase 3 – write the second half using the current V2 path.
///   Phase 4 – assert that every header (V1 or V2 on-disk) deserialises to the
///              correct height.
#[tokio::test(flavor = "multi_thread")]
async fn v1_0_to_v1_1_mixed_blockheaderdata_formats() {
    init_tracing();

    let TestVectorData { blocks, .. } = load_test_vectors().unwrap();

    // Split at the midpoint so the DB will eventually hold both encoding formats.
    let split = blocks.len() / 2;
    let first_half = blocks[..split].to_vec();
    let second_half = blocks[split..].to_vec();

    let temp_dir: TempDir = tempfile::tempdir().unwrap();
    let db_path: PathBuf = temp_dir.path().to_path_buf();

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

    // ── Phase 1: build first half (V2 write path), collect headers, downgrade metadata ──

    // Spawn the v1 backend directly so we can call `get_block_header` without
    // going through ZainoDB's migration logic.
    let db = DbBackend::spawn_v1(&v1_config).await.unwrap();
    crate::chain_index::tests::vectors::sync_db_with_blockdata(&db, first_half.clone(), None).await;

    // Wait for the background validator to mark all first-half blocks as known-good
    // so that `get_block_header` can take the fast path.
    db.wait_until_ready().await;

    // Read back the decoded headers; we will re-encode them as V1 below.
    let mut first_half_headers: Vec<(Height, BlockHeaderData)> = Vec::new();
    for block_data in &first_half {
        let h = Height(block_data.height);
        let header = db.get_block_header(h).await.unwrap();
        first_half_headers.push((h, header));
    }

    // Downgrade stored metadata to 1.0.0 so ZainoDB::spawn will trigger the
    // minor migration when we reopen.
    let mut meta = db.get_metadata().await.unwrap();
    meta.version = DbVersion {
        major: 1,
        minor: 0,
        patch: 0,
    };
    meta.schema_hash = [0u8; 32]; // stale hash; migration will refresh it
    db.update_metadata(meta).await.unwrap();

    db.shutdown().await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // ── Phase 2: overwrite first-half headers with correctly-checksummed V1 bytes ──
    //
    // We open the LMDB environment directly (the ZainoDB is shut down, so there is
    // no active writer) and replace each header entry with bytes that carry a V1-
    // encoded BlockHeaderData (BlockIndex with *optional* height) and a checksum
    // computed over those V1 bytes.  After this the `headers_1_0_0` table holds:
    //
    //   heights  0 .. split-1  → StoredEntryVar with V1-encoded BlockHeaderData
    //   heights  split .. N-1  → (not yet written)
    {
        use lmdb::{Environment, EnvironmentFlags, Transaction as _, WriteFlags};

        let lmdb_path = db_path.join("regtest").join("v1");
        let env = Environment::new()
            .set_max_dbs(12)
            .set_map_size(128 * 1024 * 1024) // 128 MiB – plenty for the test DB
            .set_flags(EnvironmentFlags::NO_TLS)
            .open(&lmdb_path)
            .unwrap();

        let headers_db = env.open_db(Some("headers_1_0_0")).unwrap();
        let mut txn = env.begin_rw_txn().unwrap();

        for (height, header) in &first_half_headers {
            // Key = Height::to_bytes() = [V1 tag][big-endian u32]
            let height_key = height.to_bytes().unwrap();

            // Re-encode this header using the V1 wire format:
            //   [V1 tag = 1][BlockIndex V1 body (optional height)][BlockData V1 body]
            let v1_item_bytes = header.to_bytes_with_version(version::V1).unwrap();

            // Checksum must be computed over the *exact* bytes that will be stored so
            // that StoredEntryVar::verify() finds a match on its V1 iteration.
            // checksum = blake2b256(height_key || v1_item_bytes)
            let checksum = StoredEntryVar::<BlockHeaderData>::blake2b256(
                &[height_key.as_slice(), v1_item_bytes.as_slice()].concat(),
            );

            // Build the full LMDB value for a StoredEntryVar<BlockHeaderData>:
            //   [StoredEntry V1 outer tag][CompactSize(item_len)][item_bytes][32-byte checksum]
            //
            // The StoredEntry outer tag is always V1 here because StoredEntryVar::VERSION = V1.
            // The "V1/V2" distinction lives *inside* item_bytes (the first byte of item_bytes
            // is the BlockHeaderData version tag).
            let mut stored_bytes: Vec<u8> = Vec::new();
            stored_bytes.push(version::V1); // StoredEntryVar outer version tag
            CompactSize::write(&mut stored_bytes, v1_item_bytes.len()).unwrap();
            stored_bytes.extend_from_slice(&v1_item_bytes);
            stored_bytes.extend_from_slice(&checksum);

            // Overwrite the existing V2 entry with the V1 one.
            txn.put(headers_db, &height_key, &stored_bytes, WriteFlags::empty())
                .unwrap();
        }

        txn.commit().unwrap();
        env.sync(true).unwrap();
    }

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // ── Phase 3: reopen at v1.1.0 – migration runs; write second half (V2) ──
    //
    // ZainoDB detects current_version 1.0.0 < target 1.1.0 and runs
    // Migration1_0_0To1_1_0, which is metadata-only and leaves the headers table
    // (including our freshly-written V1 entries) untouched.
    let zaino_db = ZainoDB::spawn(v1_config.clone(), source.clone())
        .await
        .unwrap();
    zaino_db.wait_until_ready().await;

    // Confirm the metadata migration completed correctly.
    let post_meta = zaino_db.get_metadata().await.unwrap();
    assert_eq!(
        post_meta.version,
        DbVersion {
            major: 1,
            minor: 1,
            patch: 0,
        },
        "DB version should be 1.1.0 after migration"
    );
    assert_eq!(
        post_meta.migration_status,
        MigrationStatus::Empty,
        "Migration status should be Empty after the metadata-only migration"
    );
    assert_eq!(
        post_meta.schema_hash, DB_SCHEMA_V1_HASH,
        "Schema hash should be refreshed to the current value"
    );

    // Write the second half; these use the current (V2) BlockHeaderData format,
    // so the DB now holds V1 headers at 0..split-1 and V2 headers at split..N-1.
    crate::chain_index::tests::vectors::sync_db_with_blockdata(
        zaino_db.router(),
        second_half.clone(),
        None,
    )
    .await;
    zaino_db.wait_until_ready().await;

    // ── Phase 4: verify every header decodes correctly with the right height ──
    //
    // The initial block scan in the background task has already called
    // validate_block_blocking() for every height, which internally invokes
    // StoredEntryVar::verify().  For V1-encoded entries that method tries V2 first
    // (no match), then V1 (match).  Reaching Ready status here means all blocks
    // passed validation.
    let zaino_db = Arc::new(zaino_db);
    let reader = zaino_db.to_reader();

    // V1-format headers (first half) – these were written before the format bump.
    for block_data in &first_half {
        let h = Height(block_data.height);
        let header = reader
            .get_block_header(h)
            .await
            .unwrap_or_else(|e| panic!("failed to read V1-format header at height {}: {e}", h.0));
        assert_eq!(
            header.index().height(),
            h,
            "V1-format header at height {} returned wrong height",
            h.0
        );
    }

    // V2-format headers (second half) – written after the migration.
    for block_data in &second_half {
        let h = Height(block_data.height);
        let header = reader
            .get_block_header(h)
            .await
            .unwrap_or_else(|e| panic!("failed to read V2-format header at height {}: {e}", h.0));
        assert_eq!(
            header.index().height(),
            h,
            "V2-format header at height {} returned wrong height",
            h.0
        );
    }

    // The overall DB tip should cover the full chain.
    let db_height = zaino_db.db_height().await.unwrap().unwrap();
    let expected_tip = Height((blocks.len() - 1) as u32);
    assert_eq!(
        db_height, expected_tip,
        "DB tip should be the last block's height after building the full chain"
    );

    zaino_db.shutdown().await.unwrap();
}
