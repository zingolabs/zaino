//! Zaino-State ChainIndex unit tests.

use zaino_chain_head::ChainHeadSnapshot as _;
mod chain_head;
mod mockchain_tests;
mod poll;
mod proptest_blockgen;
mod sync_loop;
pub(crate) mod vectors;

pub(crate) fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_timer(tracing_subscriber::fmt::time::UtcTime::rfc_3339())
        .with_target(true)
        .try_init()
        .unwrap();
}

use std::path::{Path, PathBuf};
use tempfile::TempDir;
use tokio::sync::OnceCell;
use tokio::time::Duration;
use zaino_common::{network::ActivationHeights, DatabaseConfig, StorageConfig};

use crate::chain_index::chain_store::WithChainStoreSource as _;
use crate::{
    chain_index::{
        finalized_height_floor,
        tests::vectors::MockSource,
        tests::vectors::{
            build_active_mockchain_source, build_mockchain_source, copy_dir_recursive,
            load_test_vectors,
        },
        ChainIndex, NodeBackedChainIndex, NodeBackedChainIndexSubscriber, SyncTimings,
    },
    ChainIndexConfig,
};

/// Selects which factory the test setup uses to build its `MockSource`,
/// which in turn determines the source's `active_chain_height` and so the
/// indexer's sync target.
///
/// - `Active` → `build_active_mockchain_source(150, blocks)`: source has a
///   separately-tracked `active_height = 150` that tests can advance via
///   `mockchain.source().mine_blocks(N)`. Indexer's finalised sync target is
///   `finalized_height_floor(150) = 50`.
/// - `Static` → `build_mockchain_source(blocks)`: every loaded block is
///   immediately active (`active_height = tip_height = 200` for the 201-block
///   vector); the tip doesn't move during the test. Indexer's finalised sync
///   target is `finalized_height_floor(200) = 100`.
/// - `StaticDeepFinalised` → `Static`'s source, but with the finalised DB seeded to
///   [`DEEP_FINALISED_SEED_TIP`] instead of the floor. `finalized_height_floor` is a
///   *lower* bound on the finalised tip, not a ceiling — `sync_to_height` short-circuits
///   on a DB already at or above its target — so this is a state the indexer serves
///   normally. It is the only mode in which the finalised index holds a transparent
///   spend: the corpus creates its first at height 102, two blocks above the deepest
///   floor a 201-block chain can reach.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MockchainMode {
    Active,
    Static,
    StaticDeepFinalised,
}

/// Finalised-DB tip for [`MockchainMode::StaticDeepFinalised`]: above the corpus's
/// earliest transparent spends and below its latest, so a scoped lookup can tell the two
/// apart.
const DEEP_FINALISED_SEED_TIP: u32 = 150;

fn mockchain_source(mode: MockchainMode, blocks: Vec<vectors::TestVectorBlockData>) -> MockSource {
    match mode {
        MockchainMode::Active => build_active_mockchain_source(150, blocks),
        MockchainMode::Static | MockchainMode::StaticDeepFinalised => {
            build_mockchain_source(blocks)
        }
    }
}

/// Height the mode's cached finalised seed DB is synced to.
fn finalised_seed_tip(mode: MockchainMode, active_height: u32) -> u32 {
    match mode {
        MockchainMode::Active | MockchainMode::Static => finalized_height_floor(active_height).0,
        MockchainMode::StaticDeepFinalised => DEEP_FINALISED_SEED_TIP,
    }
}

async fn load_test_vectors_and_sync_chain_index(
    mode: MockchainMode,
) -> (
    Vec<vectors::TestVectorBlockData>,
    NodeBackedChainIndex<MockSource>,
    NodeBackedChainIndexSubscriber<MockSource>,
    MockSource,
) {
    // 25 ms setup-poll interval mirrors `_with_timings`. The previous 2 s
    // value was load-bearing for the teardown race tracked in #1098: most
    // callers (mockchain_tests, mempool, poll, proptest_blockgen) drop the
    // indexer without calling `shutdown()`, and the old worker needed to
    // be parked in its post-success interval-sleep before runtime teardown
    // raced a mid-iter LMDB write. With `Drop for NodeBackedChainIndex`
    // firing `cancel_token.cancel()` and the worker's iter body wrapped in
    // `tokio::select!` against that token, the worker now exits at its
    // next await checkpoint on drop — the harness no longer needs to
    // bait the timing.
    load_with_settings(mode, SyncTimings::default(), Duration::from_millis(25)).await
}

async fn load_test_vectors_and_sync_chain_index_with_timings(
    mode: MockchainMode,
    sync_timings: SyncTimings,
) -> (
    Vec<vectors::TestVectorBlockData>,
    NodeBackedChainIndex<MockSource>,
    NodeBackedChainIndexSubscriber<MockSource>,
    MockSource,
) {
    load_with_settings(mode, sync_timings, Duration::from_millis(25)).await
}

async fn load_with_settings(
    mode: MockchainMode,
    sync_timings: SyncTimings,
    setup_poll_interval: Duration,
) -> (
    Vec<vectors::TestVectorBlockData>,
    NodeBackedChainIndex<MockSource>,
    NodeBackedChainIndexSubscriber<MockSource>,
    MockSource,
) {
    init_tracing();

    let blocks = load_test_vectors().unwrap().blocks;

    let source = mockchain_source(mode, blocks.clone());

    let temp_dir: TempDir = tempfile::tempdir().unwrap();
    let db_path: PathBuf = temp_dir.path().to_path_buf();

    // Seed the temp DB from a process-wide pre-synced fixture. The fixture is
    // built once per mode (see `v1_finalised_seed_dir`) and synced to exactly
    // the height the indexer's sync loop would target here, so spawning the
    // indexer against this copy hits a no-op `sync_to_height` and the wait
    // loop below completes on its first probe rather than after a fresh
    // ingest of every test-vector block.
    let seed = v1_finalised_seed_dir(mode).await;
    copy_dir_recursive(seed, &db_path).unwrap();

    let config = ChainIndexConfig {
        storage: StorageConfig {
            database: DatabaseConfig {
                path: db_path,
                ..Default::default()
            },
            ..Default::default()
        },
        ephemeral: false,
        mempool: Default::default(),
        db_version: 1,
        network: ActivationHeights::default().to_regtest_network(),
    };

    let indexer = NodeBackedChainIndex::new_with_sync_timings(source.clone(), config, sync_timings)
        .await
        .unwrap();
    let index_reader = indexer.subscriber();

    // Wait until the indexer's non-finalised state has been built and its
    // best tip matches the source. The previous form checked only
    // `finalized_state.db_height() == finalized_height_floor(active_height)`,
    // which the seed copy makes true *before* the sync loop has had a chance
    // to initialise NFS. Tests that read the NFS immediately after the
    // helper returns (`nfs_lowest_block_matches_finalized_db_tip`,
    // `sync_blocks_after_startup`, …) then unwrap on `None`. The NFS being
    // at `source.source().active_height()` implies the finalised DB has reached its
    // floor — the sync loop only initialises NFS after `sync_to_height`
    // succeeds — so this condition subsumes the old one.
    let expected_nfs_tip = source.source().active_height();
    // Bound the readiness wait so a sync worker that never signals NFS-ready
    // (a starvation / missed-notification hang in chain-index sync, observed on
    // this helper under full-suite parallelism) fails loud here instead of
    // hanging the whole test indefinitely. The seed copy normally satisfies the
    // condition on the first probe (~0.5s), so 10s is ~20x the expected margin
    // — only a genuine hang trips it.
    const NFS_READY_BUDGET: Duration = Duration::from_secs(10);
    tokio::time::timeout(NFS_READY_BUDGET, async {
        loop {
            let nfs_ready = u32::from(index_reader.snapshot_nonfinalized_state().best_tip().height)
                == expected_nfs_tip;
            if nfs_ready {
                break;
            }
            tokio::time::sleep(setup_poll_interval).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "chain-index sync worker did not bring non-finalised state to the \
             expected tip (height {expected_nfs_tip}) within {NFS_READY_BUDGET:?}; \
             the worker likely deadlocked or missed its readiness notification \
             under load (chain index integration)"
        )
    });

    (blocks, indexer, index_reader, source)
}

/// Process-wide cached, fully-synced v1 finalised-state databases — one per
/// `MockchainMode`. The modes target different heights (Active → 50,
/// Static → 100, StaticDeepFinalised → [`DEEP_FINALISED_SEED_TIP`]), so they
/// need distinct seeds.
///
/// Built lazily on first call via `tokio::sync::OnceCell`, which serialises
/// the build under concurrent test access. Each test still gets an isolated
/// writable DB by copying the seed dir into its own tempdir (see
/// `copy_dir_recursive`); the seed itself is never mutated after first build.
static V1_SEED_ACTIVE: OnceCell<TempDir> = OnceCell::const_new();
static V1_SEED_STATIC: OnceCell<TempDir> = OnceCell::const_new();
static V1_SEED_STATIC_DEEP_FINALISED: OnceCell<TempDir> = OnceCell::const_new();

async fn v1_finalised_seed_dir(mode: MockchainMode) -> &'static Path {
    let cell = match mode {
        MockchainMode::Active => &V1_SEED_ACTIVE,
        MockchainMode::Static => &V1_SEED_STATIC,
        MockchainMode::StaticDeepFinalised => &V1_SEED_STATIC_DEEP_FINALISED,
    };
    cell.get_or_init(|| async move {
        let blocks = load_test_vectors().unwrap().blocks;
        let source = mockchain_source(mode, blocks.clone());
        let target = finalised_seed_tip(mode, source.source().active_height());

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
            mempool: Default::default(),
            db_version: 1,
            network: ActivationHeights::default().to_regtest_network(),
        };

        // Filled block-by-block rather than through `build_to`: the seed only
        // needs to *be* a database at `target`, and driving the store's ingest
        // to get there makes every test process pay for the background
        // validator the batch write path wakes up — enough, under a parallel
        // runner, to dominate the suite's runtime.
        let zaino_db = zaino_chain_store_zainodb::store::FinalisedState::spawn(
            config.chain_store_config(),
            config.zainodb_config(),
            source.chain_store_source(),
        )
        .await
        .unwrap();
        zaino_chain_store_zainodb::tests::fixtures::fill_store_with_blockdata(
            &zaino_db,
            &blocks,
            Some(target),
        )
        .await;
        zaino_db.wait_until_ready().await;
        zaino_db.shutdown().await.unwrap();

        temp_dir
    })
    .await
    .path()
}
