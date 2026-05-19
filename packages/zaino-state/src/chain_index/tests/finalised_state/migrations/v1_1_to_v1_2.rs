//! V1.1 to V1.2 migration tests.

use lmdb::{Cursor as _, Transaction as _, WriteFlags};
use std::path::PathBuf;
use tempfile::TempDir;
use zaino_common::network::ActivationHeights;
use zaino_common::{DatabaseConfig, Network, StorageConfig};

use crate::chain_index::finalised_state::capability::{
    BlockCoreExt as _, BlockTransparentExt as _, CapabilityRequest, DbRead as _, DbVersion,
    MigrationStatus, TransparentHistExt as _,
};
use crate::chain_index::finalised_state::db::v1::{
    DB_SCHEMA_V1_HASH, TX_OUT_SET_INFO_ACCUMULATOR_KEY,
};
use crate::chain_index::finalised_state::db::DbBackend;
use crate::chain_index::finalised_state::entry::StoredEntryFixed;
use crate::chain_index::finalised_state::ZainoDB;
use crate::chain_index::tests::init_tracing;
use crate::chain_index::tests::vectors::{
    build_active_mockchain_source, load_test_vectors, TestVectorData,
};
use crate::chain_index::types::db::metadata::FinalisedTxOutSetInfoAccumulator;
use crate::{BlockCacheConfig, Height, Outpoint, TxLocation, ZainoVersionedSerde as _};

const MIGRATION_SPENT_PROGRESS_KEY: &[u8] = b"_migration_spent_progress_1_2_0_next_height";

fn v1_1_0() -> DbVersion {
    DbVersion {
        major: 1,
        minor: 1,
        patch: 0,
    }
}

fn v1_2_0() -> DbVersion {
    DbVersion {
        major: 1,
        minor: 2,
        patch: 0,
    }
}

async fn assert_v1_2_migration_complete(zaino_database: &ZainoDB) {
    let metadata = zaino_database.get_metadata().await.unwrap();

    assert_eq!(metadata.version, v1_2_0());
    assert_eq!(metadata.migration_status, MigrationStatus::Empty);
    assert_eq!(metadata.schema_hash, DB_SCHEMA_V1_HASH);

    assert!(
        zaino_database
            .router()
            .backend(CapabilityRequest::TransparentHistExt)
            .is_ok(),
        "v1.2.0 database should expose TransparentHistExt after migration"
    );
}

async fn simulate_interrupted_v1_1_to_v1_2_spent_index_migration(
    database_backend: &DbBackend,
    resume_height: Height,
) {
    let environment = database_backend.env();
    let metadata_database = database_backend.metadata_db().unwrap();
    let spent_database = database_backend.spent_db().unwrap();
    let tx_out_set_info_accumulator_database =
        database_backend.tx_out_set_info_accumulator_db().unwrap();

    let expected_resume_accumulator =
        expected_tx_out_set_info_accumulator(database_backend, resume_height - 1).await;

    let spent_keys_to_delete: Vec<Vec<u8>> = {
        let transaction = environment.begin_ro_txn().unwrap();
        let mut cursor = transaction.open_ro_cursor(spent_database).unwrap();
        let mut spent_keys_to_delete = Vec::new();
        let mut kept_spent_entry_count = 0usize;

        for (outpoint_bytes, spent_bytes) in cursor.iter_start() {
            let spent_entry = StoredEntryFixed::<TxLocation>::from_bytes(spent_bytes).unwrap();

            assert!(
                spent_entry.verify(outpoint_bytes),
                "spent entry checksum mismatch before interrupted migration setup"
            );

            if spent_entry.inner().block_height() >= resume_height.0 {
                spent_keys_to_delete.push(outpoint_bytes.to_vec());
            } else {
                kept_spent_entry_count += 1;
            }
        }

        assert!(
            kept_spent_entry_count > 0,
            "interrupted migration setup did not keep any spent entries before resume height {}",
            resume_height.0
        );

        assert!(
            !spent_keys_to_delete.is_empty(),
            "interrupted migration setup did not find any spent entries to delete at or after resume height {}",
            resume_height.0
        );

        spent_keys_to_delete
    };

    let mut metadata = database_backend.get_metadata().await.unwrap();
    metadata.version = v1_1_0();
    metadata.schema_hash = [0u8; 32];
    metadata.migration_status = MigrationStatus::PartialBuidInProgress;

    {
        let mut transaction = environment.begin_rw_txn().unwrap();

        for spent_key in spent_keys_to_delete {
            transaction.del(spent_database, &spent_key, None).unwrap();
        }

        let tx_out_set_info_accumulator_entry =
            StoredEntryFixed::new(TX_OUT_SET_INFO_ACCUMULATOR_KEY, expected_resume_accumulator);

        transaction
            .put(
                tx_out_set_info_accumulator_database,
                &TX_OUT_SET_INFO_ACCUMULATOR_KEY,
                &tx_out_set_info_accumulator_entry.to_bytes().unwrap(),
                WriteFlags::empty(),
            )
            .unwrap();

        let progress_entry = StoredEntryFixed::new(MIGRATION_SPENT_PROGRESS_KEY, resume_height);
        let progress_bytes = progress_entry.to_bytes().unwrap();

        transaction
            .put(
                metadata_database,
                &MIGRATION_SPENT_PROGRESS_KEY,
                &progress_bytes,
                WriteFlags::empty(),
            )
            .unwrap();

        let metadata_key = b"metadata";
        let metadata_bytes = StoredEntryFixed::new(metadata_key, metadata)
            .to_bytes()
            .unwrap();

        transaction
            .put(
                metadata_database,
                metadata_key,
                &metadata_bytes,
                WriteFlags::empty(),
            )
            .unwrap();

        transaction.commit().unwrap();
    }

    environment.sync(true).unwrap();
}

async fn assert_spent_index_matches_transparent_data(database_backend: &DbBackend) {
    let environment = database_backend.env();
    let spent_database = database_backend.spent_db().unwrap();

    let database_height = database_backend.db_height().await.unwrap().unwrap();

    for height_raw in 0..=database_height.0 {
        let height = Height(height_raw);
        let transparent_transaction_list = database_backend
            .get_block_transparent(height)
            .await
            .unwrap();

        let transaction = environment.begin_ro_txn().unwrap();

        for (transaction_index, transparent_transaction_opt) in
            transparent_transaction_list.tx().iter().enumerate()
        {
            let Some(transparent_transaction) = transparent_transaction_opt else {
                continue;
            };

            let expected_transaction_location = TxLocation::new(height.0, transaction_index as u16);

            for input in transparent_transaction.inputs() {
                if input.is_null_prevout() {
                    continue;
                }

                let outpoint = Outpoint::new(*input.prevout_txid(), input.prevout_index());
                let outpoint_bytes = outpoint.to_bytes().unwrap();

                let spent_bytes = transaction
                    .get(spent_database, &outpoint_bytes)
                    .unwrap_or_else(|error| {
                        panic!(
                            "missing spent entry for outpoint {:?} spent at height {}: {error}",
                            outpoint, height.0
                        )
                    });

                let spent_entry = StoredEntryFixed::<TxLocation>::from_bytes(spent_bytes)
                    .unwrap_or_else(|error| {
                        panic!(
                            "corrupt spent entry for outpoint {:?} spent at height {}: {error}",
                            outpoint, height.0
                        )
                    });

                assert!(
                    spent_entry.verify(&outpoint_bytes),
                    "spent checksum mismatch for outpoint {:?} spent at height {}",
                    outpoint,
                    height.0
                );

                assert_eq!(
                    spent_entry.inner(),
                    &expected_transaction_location,
                    "spent entry points to wrong TxLocation for outpoint {:?}",
                    outpoint
                );
            }
        }
    }
}

async fn expected_tx_out_set_info_accumulator(
    database_backend: &DbBackend,
    max_height: Height,
) -> FinalisedTxOutSetInfoAccumulator {
    let environment = database_backend.env();
    let spent_database = database_backend.spent_db().unwrap();

    let mut expected_accumulator = FinalisedTxOutSetInfoAccumulator::empty();

    for height_raw in 0..=max_height.0 {
        let height = Height(height_raw);

        let transparent_transaction_list = database_backend
            .get_block_transparent(height)
            .await
            .unwrap();

        for (transaction_index, transparent_transaction_opt) in
            transparent_transaction_list.tx().iter().enumerate()
        {
            let Some(transparent_transaction) = transparent_transaction_opt else {
                continue;
            };

            if transparent_transaction.outputs().is_empty() {
                continue;
            }

            let transaction_index = u16::try_from(transaction_index).unwrap();
            let transaction_location = TxLocation::new(height.0, transaction_index);

            let transaction_hash = database_backend
                .get_txid(transaction_location)
                .await
                .unwrap();

            let mut unspent_outputs_for_transaction = 0u64;

            let transaction = environment.begin_ro_txn().unwrap();

            for (output_index, output) in transparent_transaction.outputs().iter().enumerate() {
                // The accumulator excludes NonStandard (unspendable) outputs from every field —
                // see `is_unspendable_tx_out`. The migration oracle must skip them too,
                // otherwise it overcounts compared to the on-disk accumulator value the
                // migration backfilled.
                if crate::chain_index::types::db::metadata::is_unspendable_tx_out(output) {
                    continue;
                }

                let output_index = u32::try_from(output_index).unwrap();
                let outpoint = Outpoint::new(transaction_hash.0, output_index);
                let outpoint_bytes = outpoint.to_bytes().unwrap();

                let still_unspent = match transaction.get(spent_database, &outpoint_bytes) {
                    Ok(spent_bytes) => {
                        let spent_entry =
                            StoredEntryFixed::<TxLocation>::from_bytes(spent_bytes).unwrap();

                        assert!(
                            spent_entry.verify(&outpoint_bytes),
                            "spent checksum mismatch for outpoint {:?}",
                            outpoint
                        );

                        spent_entry.inner().block_height() > max_height.0
                    }

                    Err(lmdb::Error::NotFound) => true,

                    Err(error) => panic!(
                        "failed to read spent entry for outpoint {:?}: {error}",
                        outpoint
                    ),
                };

                if still_unspent {
                    unspent_outputs_for_transaction += 1;
                    expected_accumulator
                        .apply_added_output(&outpoint, output)
                        .unwrap();
                }
            }

            if unspent_outputs_for_transaction > 0 {
                expected_accumulator.transactions += 1;
            }
        }
    }

    expected_accumulator
}

async fn assert_tx_out_set_info_accumulator_matches_transparent_data(database_backend: &DbBackend) {
    let database_height = database_backend.db_height().await.unwrap().unwrap();

    let expected_accumulator =
        expected_tx_out_set_info_accumulator(database_backend, database_height).await;

    let actual_accumulator = database_backend
        .get_tx_out_set_info_accumulator()
        .await
        .unwrap();

    assert_eq!(
        actual_accumulator, expected_accumulator,
        "txout-set accumulator does not match transparent data and spent index"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn v1_1_to_v1_2_spent_index_backfill_from_old_version() {
    init_tracing();

    let TestVectorData { blocks, .. } = load_test_vectors().unwrap();

    let initial_active_height = Height(150);

    let temporary_directory: TempDir = tempfile::tempdir().unwrap();
    let database_path: PathBuf = temporary_directory.path().to_path_buf();

    let database_config = BlockCacheConfig {
        storage: StorageConfig {
            database: DatabaseConfig {
                path: database_path,
                ..Default::default()
            },
            ..Default::default()
        },
        db_version: 1,
        network: Network::Regtest(ActivationHeights::default()),
    };

    let source = build_active_mockchain_source(initial_active_height.0, blocks.clone());

    let old_database =
        ZainoDB::build_db_to_version(database_config.clone(), source.clone(), v1_1_0())
            .await
            .unwrap();

    old_database.wait_until_ready().await;

    let old_metadata = old_database.get_metadata().await.unwrap();
    assert_eq!(old_metadata.version, v1_1_0());
    assert_eq!(old_metadata.migration_status, MigrationStatus::Empty);
    assert_eq!(old_metadata.schema_hash, DB_SCHEMA_V1_HASH);

    let old_database_height = old_database.db_height().await.unwrap().unwrap();
    assert_eq!(old_database_height, initial_active_height);

    old_database.shutdown().await.unwrap();
    drop(old_database);

    let migrated_database =
        ZainoDB::spawn_with_target_version(database_config.clone(), source.clone(), v1_2_0())
            .await
            .unwrap();

    migrated_database.wait_until_ready().await;

    assert_v1_2_migration_complete(&migrated_database).await;

    let migrated_database_height = migrated_database.db_height().await.unwrap().unwrap();
    assert_eq!(migrated_database_height, initial_active_height);

    let migrated_backend = migrated_database
        .router()
        .backend(CapabilityRequest::WriteCore)
        .unwrap();

    assert_spent_index_matches_transparent_data(&migrated_backend).await;
    assert_tx_out_set_info_accumulator_matches_transparent_data(&migrated_backend).await;

    migrated_database.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn v1_1_to_v1_2_spent_index_migration_resumes_after_crash() {
    init_tracing();

    let TestVectorData { blocks, .. } = load_test_vectors().unwrap();

    let initial_active_height = Height(150);
    let resume_height = Height(145);

    let temporary_directory: TempDir = tempfile::tempdir().unwrap();
    let database_path: PathBuf = temporary_directory.path().to_path_buf();

    let database_config = BlockCacheConfig {
        storage: StorageConfig {
            database: DatabaseConfig {
                path: database_path,
                ..Default::default()
            },
            ..Default::default()
        },
        db_version: 1,
        network: Network::Regtest(ActivationHeights::default()),
    };

    let source = build_active_mockchain_source(initial_active_height.0, blocks.clone());

    let old_database =
        ZainoDB::build_db_to_version(database_config.clone(), source.clone(), v1_1_0())
            .await
            .unwrap();

    old_database.wait_until_ready().await;

    let old_metadata = old_database.get_metadata().await.unwrap();
    assert_eq!(old_metadata.version, v1_1_0());
    assert_eq!(old_metadata.migration_status, MigrationStatus::Empty);
    assert_eq!(old_metadata.schema_hash, DB_SCHEMA_V1_HASH);

    old_database.shutdown().await.unwrap();
    drop(old_database);

    let complete_migration_database =
        ZainoDB::spawn_with_target_version(database_config.clone(), source.clone(), v1_2_0())
            .await
            .unwrap();

    complete_migration_database.wait_until_ready().await;

    assert_v1_2_migration_complete(&complete_migration_database).await;

    {
        let complete_migration_backend = complete_migration_database
            .router()
            .backend(CapabilityRequest::WriteCore)
            .unwrap();

        simulate_interrupted_v1_1_to_v1_2_spent_index_migration(
            &complete_migration_backend,
            resume_height,
        )
        .await;
    }

    complete_migration_database.shutdown().await.unwrap();
    drop(complete_migration_database);

    let resumed_database =
        ZainoDB::spawn_with_target_version(database_config.clone(), source.clone(), v1_2_0())
            .await
            .unwrap();

    resumed_database.wait_until_ready().await;

    assert_v1_2_migration_complete(&resumed_database).await;

    let resumed_database_height = resumed_database.db_height().await.unwrap().unwrap();
    assert_eq!(resumed_database_height, initial_active_height);

    let resumed_backend = resumed_database
        .router()
        .backend(CapabilityRequest::WriteCore)
        .unwrap();

    assert_spent_index_matches_transparent_data(&resumed_backend).await;
    assert_tx_out_set_info_accumulator_matches_transparent_data(&resumed_backend).await;

    resumed_database.shutdown().await.unwrap();
}
