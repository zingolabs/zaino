//! V1.2.1 to V1.3.0 migration tests.
//!
//! Coverage note: these tests use `ActivationHeights::default()`, whose NU6.3 activation is `None`,
//! so the migration takes the below-activation branch (rebuild each commitment row in place from the
//! legacy fixed-length table, no ironwood). The at/above-activation *ironwood backfill* branch —
//! which refetches block data from the validator — cannot be exercised yet: `MockchainSource`
//! serves no ironwood commitment roots (see its `get_commitment_tree_roots` TODO) and the test
//! vectors carry no ironwood actions, so building a post-NU6.3 block would fail resolving the
//! (required) ironwood root. That path needs ironwood-capable test vectors before it can be tested.

use std::path::PathBuf;
use tempfile::TempDir;
use zaino_common::network::ActivationHeights;
use zaino_common::{DatabaseConfig, StorageConfig};

use crate::chain_index::finalised_state::capability::{DbVersion, MigrationStatus};
use crate::chain_index::finalised_state::finalised_source::v1::DB_VERSION_V1;
use crate::chain_index::finalised_state::FinalisedState;
use crate::chain_index::tests::init_tracing;
use crate::chain_index::tests::vectors::{
    build_active_mockchain_source, load_test_vectors, TestVectorData,
};
use crate::{ChainIndexConfig, Height, StatusType};

fn v1_2_1() -> DbVersion {
    DbVersion {
        major: 1,
        minor: 2,
        patch: 1,
    }
}

/// Regression test for the startup ordering bug: opening a pre-1.3.0 cache used to start the
/// background validator concurrently with the v1.2.1 → v1.3.0 migration. The validator's
/// `initial_block_scan` read the freshly-created-empty `commitment_tree_data_1_3_0` table before the
/// migration rebuilt it, failing with `MDB_NOTFOUND` ("block scan") and latching `CriticalError`.
///
/// The fix defers the validator until every migration finishes. This test builds an on-disk v1.2.1
/// database, then reopens it through the production `FinalisedState::spawn` path (which migrates to
/// the current schema and only then starts the validator) and asserts it reaches `Ready` — i.e. the
/// migration completed *before* validation, and validation then passed.
// multi_thread required: the background validator runs blocking LMDB validation
// (`validate_block_blocking`) inline on its task, which would starve this test's status polling on
// a current-thread runtime.
#[tokio::test(flavor = "multi_thread")]
async fn v1_2_1_cache_migrates_to_current_then_validates() {
    init_tracing();

    let TestVectorData { blocks, .. } = load_test_vectors().unwrap();
    let active_height = Height(150);

    let temporary_directory: TempDir = tempfile::tempdir().unwrap();
    let database_path: PathBuf = temporary_directory.path().to_path_buf();

    let database_config = ChainIndexConfig {
        storage: StorageConfig {
            database: DatabaseConfig {
                path: database_path,
                ..Default::default()
            },
            ..Default::default()
        },
        ephemeral: false,
        db_version: 1,
        network: ActivationHeights::default().to_regtest_network(),
    };

    let source = build_active_mockchain_source(active_height.0, blocks.clone());

    // Build an on-disk v1.2.1 database (the pre-1.3.0 cache shape), then release it.
    let old_database =
        FinalisedState::build_db_to_version(database_config.clone(), source.clone(), v1_2_1())
            .await
            .unwrap();
    old_database.wait_until_synced().await;
    assert_eq!(old_database.get_metadata().await.unwrap().version, v1_2_1());
    old_database.shutdown().await.unwrap();
    drop(old_database);

    // Reopen through the production spawn path: it runs the v1.2.1 → v1.3.0 migration and only then
    // starts the validator. Before the ordering fix this raced the migration and latched
    // `CriticalError`; now it must migrate first and validate cleanly.
    let migrated_database = FinalisedState::spawn(database_config.clone(), source.clone())
        .await
        .unwrap();
    migrated_database.wait_until_synced().await;

    assert_eq!(
        migrated_database.status(),
        StatusType::Ready,
        "the validator must run only after the migration completes, and then reach Ready"
    );

    let migrated_metadata = migrated_database.get_metadata().await.unwrap();
    assert_eq!(migrated_metadata.version, DB_VERSION_V1);
    assert_eq!(migrated_metadata.migration_status, MigrationStatus::Empty);

    let migrated_height = migrated_database.db_height().await.unwrap().unwrap();
    assert_eq!(migrated_height, active_height);

    migrated_database.shutdown().await.unwrap();
}
