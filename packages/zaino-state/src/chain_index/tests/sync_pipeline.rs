//! Concurrency tests for the look-ahead fetch pipeline in
//! [`ZainoDB::sync_to_height`](crate::chain_index::finalised_state::ZainoDB::sync_to_height).
//!
//! The pipeline fetches up to `storage.database.sync_fetch_lookahead` blocks concurrently and
//! out of order, but build+write must stay strictly height-ordered (`parent_chainwork` threads
//! sequentially and the writer demands height-contiguity). Every test here targets one
//! question: does out-of-order *fetching* ever leak into out-of-order — or mis-paired —
//! *building*, and is the configured concurrency actually respected?
//!
//! The enabling piece is [`FaultInjectingSource`], a thin `BlockchainSource` decorator that
//! wraps the existing mockchain and injects per-height fetch latency, targeted errors, and a
//! live in-flight-fetch counter. It touches no production code.
//!
//! Covered here:
//! - [`pipeline_out_of_order_fetch_builds_in_order_db`] — reverse per-height latency makes
//!   later heights' fetches complete first; the resulting DB must still be byte-equivalent
//!   to an in-order (golden) sync. (Plan Tier-1 #2.)
//! - [`pipeline_getblock_error_propagates_without_hang`] /
//!   [`pipeline_treestate_error_propagates_without_hang`] — an injected error on either fetch
//!   leg must surface as `Err`, must not deadlock, and must not commit past the failing
//!   height. (Plan Tier-3 #6.)
//! - [`pipeline_respects_configured_lookahead_bound`] — concurrent in-flight fetches never
//!   exceed `storage.database.sync_fetch_lookahead`, and do exceed one (it really pipelines).
//! - [`lookahead_of_one_is_serial_and_correct`] — depth 1 degrades to a correct serial sync.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tempfile::TempDir;
use zaino_common::network::ActivationHeights;
use zaino_common::{DatabaseConfig, Network, StorageConfig};
use zebra_rpc::client::{GetAddressBalanceRequest, GetAddressTxIdsRequest};
use zebra_rpc::methods::{AddressBalance, GetAddressUtxos};
use zebra_state::HashOrHeight;

use zaino_fetch::jsonrpsee::response::address_deltas::{
    GetAddressDeltasParams, GetAddressDeltasResponse,
};

use super::vectors::{build_mockchain_source, load_test_vectors};
use crate::chain_index::finalised_state::reader::DbReader;
use crate::chain_index::finalised_state::ZainoDB;
use crate::chain_index::source::{
    BlockchainSource, BlockchainSourceError, BlockchainSourceResult, GetTransactionLocation,
};
use crate::chain_index::types::{BlockHash, Height, TransactionHash};
use crate::chain_index::ShieldedPool;
use crate::BlockCacheConfig;

/// How many vector blocks each test syncs. Comfortably larger than the pipeline's look-ahead
/// window so the reorder buffer is actually exercised.
const TIP: u32 = 60;

// ***** Fault-injecting BlockchainSource decorator *****

/// Per-source fault configuration, shared by clones via `Arc`.
struct Faults {
    /// Latency applied to `get_block(Height(h))` before delegating. Used to force later
    /// heights to complete before earlier ones.
    block_delay_by_height: Box<dyn Fn(u32) -> Duration + Send + Sync>,
    /// If set, `get_block(Height(N))` returns `Err` — the getblock leg of the error tests.
    fail_block_at_height: Option<u32>,
    /// If set, `get_commitment_tree_roots(hash)` returns `Err` when `hash` matches — the
    /// treestate leg. Keyed by hash because the trait method is hash-addressed; tests
    /// precompute the target height's hash exactly as the sync loop derives it.
    fail_treestate_for_hash: Option<BlockHash>,
}

/// Live counter of concurrent in-flight `get_block` calls, shared by clones via `Arc`. Lets
/// the bounded-concurrency test observe the pipeline's actual fetch parallelism.
#[derive(Default)]
struct ConcurrencyMeter {
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
}

/// RAII guard: counts one in-flight fetch for its lifetime, decrementing on drop so the count
/// stays correct even when a fetch returns early with an error. Holds an `Arc` clone so it
/// carries no borrow of the fetch method's `&self` across the await.
struct InFlightGuard(Arc<ConcurrencyMeter>);

impl InFlightGuard {
    fn enter(meter: &Arc<ConcurrencyMeter>) -> Self {
        let now = meter.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        meter.max_in_flight.fetch_max(now, Ordering::SeqCst);
        Self(Arc::clone(meter))
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Wraps a `BlockchainSource` and injects latency / errors on the two fetch legs the sync
/// pipeline uses (`get_block`, `get_commitment_tree_roots`); every other method delegates.
#[derive(Clone)]
struct FaultInjectingSource<S: BlockchainSource> {
    inner: S,
    faults: Arc<Faults>,
    meter: Arc<ConcurrencyMeter>,
}

impl<S: BlockchainSource> FaultInjectingSource<S> {
    fn new(inner: S, faults: Faults) -> Self {
        Self {
            inner,
            faults: Arc::new(faults),
            meter: Arc::new(ConcurrencyMeter::default()),
        }
    }

    /// Peak number of `get_block` calls that were ever in flight at once.
    fn max_in_flight(&self) -> usize {
        self.meter.max_in_flight.load(Ordering::SeqCst)
    }

    /// No latency, no errors — the in-order "golden" baseline.
    fn no_faults(inner: S) -> Self {
        Self::new(
            inner,
            Faults {
                block_delay_by_height: Box::new(|_| Duration::ZERO),
                fail_block_at_height: None,
                fail_treestate_for_hash: None,
            },
        )
    }

    /// Reverse latency: earlier heights are slower, so within each look-ahead window the
    /// later heights' fetches complete first, forcing the ordered buffer to reorder.
    fn with_reverse_delay(inner: S, top_height: u32, unit: Duration) -> Self {
        Self::new(
            inner,
            Faults {
                block_delay_by_height: Box::new(move |h| unit * top_height.saturating_sub(h)),
                fail_block_at_height: None,
                fail_treestate_for_hash: None,
            },
        )
    }

    /// Uniform per-block fetch latency, independent of height. Makes fetches pile up so the
    /// bounded-concurrency test can observe how many run concurrently.
    fn with_uniform_delay(inner: S, delay: Duration) -> Self {
        Self::new(
            inner,
            Faults {
                block_delay_by_height: Box::new(move |_| delay),
                fail_block_at_height: None,
                fail_treestate_for_hash: None,
            },
        )
    }

    /// `get_block` fails at `height` (the getblock leg).
    fn fail_block_at(inner: S, height: u32) -> Self {
        Self::new(
            inner,
            Faults {
                block_delay_by_height: Box::new(|_| Duration::ZERO),
                fail_block_at_height: Some(height),
                fail_treestate_for_hash: None,
            },
        )
    }

    /// `get_commitment_tree_roots` fails for `hash` (the treestate leg).
    fn fail_treestate_for_hash(inner: S, hash: BlockHash) -> Self {
        Self::new(
            inner,
            Faults {
                block_delay_by_height: Box::new(|_| Duration::ZERO),
                fail_block_at_height: None,
                fail_treestate_for_hash: Some(hash),
            },
        )
    }
}

#[async_trait]
impl<S: BlockchainSource> BlockchainSource for FaultInjectingSource<S> {
    async fn get_block(
        &self,
        id: HashOrHeight,
    ) -> BlockchainSourceResult<Option<Arc<zebra_chain::block::Block>>> {
        let _in_flight = InFlightGuard::enter(&self.meter);
        if let HashOrHeight::Height(zebra_chain::block::Height(height)) = id {
            let delay = (self.faults.block_delay_by_height)(height);
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            if self.faults.fail_block_at_height == Some(height) {
                return Err(BlockchainSourceError::Unrecoverable(format!(
                    "injected getblock failure at height {height}"
                )));
            }
        }
        self.inner.get_block(id).await
    }

    async fn get_commitment_tree_roots(
        &self,
        id: BlockHash,
    ) -> BlockchainSourceResult<(
        Option<(zebra_chain::sapling::tree::Root, u64)>,
        Option<(zebra_chain::orchard::tree::Root, u64)>,
    )> {
        if self.faults.fail_treestate_for_hash == Some(id) {
            return Err(BlockchainSourceError::Unrecoverable(format!(
                "injected treestate failure for block {id}"
            )));
        }
        self.inner.get_commitment_tree_roots(id).await
    }

    // ----- pure delegations -----

    async fn get_transaction(
        &self,
        txid: TransactionHash,
    ) -> BlockchainSourceResult<
        Option<(
            Arc<zebra_chain::transaction::Transaction>,
            GetTransactionLocation,
        )>,
    > {
        self.inner.get_transaction(txid).await
    }

    async fn get_mempool_txids(
        &self,
    ) -> BlockchainSourceResult<Option<Vec<zebra_chain::transaction::Hash>>> {
        self.inner.get_mempool_txids().await
    }

    async fn get_best_block_hash(
        &self,
    ) -> BlockchainSourceResult<Option<zebra_chain::block::Hash>> {
        self.inner.get_best_block_hash().await
    }

    async fn get_best_block_height(
        &self,
    ) -> BlockchainSourceResult<Option<zebra_chain::block::Height>> {
        self.inner.get_best_block_height().await
    }

    async fn get_treestate(
        &self,
        id: BlockHash,
    ) -> BlockchainSourceResult<(Option<Vec<u8>>, Option<Vec<u8>>)> {
        self.inner.get_treestate(id).await
    }

    async fn get_subtree_roots(
        &self,
        pool: ShieldedPool,
        start_index: u16,
        max_entries: Option<u16>,
    ) -> BlockchainSourceResult<Vec<([u8; 32], u32)>> {
        self.inner
            .get_subtree_roots(pool, start_index, max_entries)
            .await
    }

    async fn get_address_deltas(
        &self,
        params: GetAddressDeltasParams,
    ) -> BlockchainSourceResult<GetAddressDeltasResponse> {
        self.inner.get_address_deltas(params).await
    }

    async fn get_address_balance(
        &self,
        address_strings: GetAddressBalanceRequest,
    ) -> BlockchainSourceResult<AddressBalance> {
        self.inner.get_address_balance(address_strings).await
    }

    async fn get_address_txids(
        &self,
        request: GetAddressTxIdsRequest,
    ) -> BlockchainSourceResult<Vec<TransactionHash>> {
        self.inner.get_address_txids(request).await
    }

    async fn get_address_utxos(
        &self,
        address_strings: GetAddressBalanceRequest,
    ) -> BlockchainSourceResult<Vec<GetAddressUtxos>> {
        self.inner.get_address_utxos(address_strings).await
    }

    async fn nonfinalized_listener(
        &self,
    ) -> Result<
        Option<
            tokio::sync::mpsc::Receiver<(zebra_chain::block::Hash, Arc<zebra_chain::block::Block>)>,
        >,
        Box<dyn std::error::Error + Send + Sync>,
    > {
        self.inner.nonfinalized_listener().await
    }

    fn subscribe_to_blocks_received(&self) -> Option<tokio::sync::watch::Receiver<()>> {
        self.inner.subscribe_to_blocks_received()
    }
}

// ***** Helpers *****

/// Spawns a fresh v1 finalised DB over `source` with the given fetch-pipeline depth.
/// (`spawn` only touches the source on the migration path, so injected faults fire only
/// during the explicit `sync_to_height` call.)
async fn spawn_finalised_db_with_lookahead<S: BlockchainSource>(
    source: S,
    sync_fetch_lookahead: usize,
) -> (TempDir, ZainoDB) {
    let temp_dir = tempfile::tempdir().unwrap();
    let config = BlockCacheConfig {
        storage: StorageConfig {
            database: DatabaseConfig {
                path: temp_dir.path().to_path_buf(),
                sync_fetch_lookahead,
                ..Default::default()
            },
            ..Default::default()
        },
        db_version: 1,
        network: Network::Regtest(ActivationHeights::default()),
    };
    let db = ZainoDB::spawn(config, source).await.unwrap();
    (temp_dir, db)
}

/// Spawns a fresh v1 finalised DB over `source` at the default pipeline depth.
async fn spawn_finalised_db<S: BlockchainSource>(source: S) -> (TempDir, ZainoDB) {
    spawn_finalised_db_with_lookahead(source, DatabaseConfig::default().sync_fetch_lookahead).await
}

/// Spawns a fresh v1 finalised DB with explicit fetch and build pipeline widths.
async fn spawn_finalised_db_with_widths<S: BlockchainSource>(
    source: S,
    sync_fetch_lookahead: usize,
    sync_build_concurrency: usize,
) -> (TempDir, ZainoDB) {
    let temp_dir = tempfile::tempdir().unwrap();
    let config = BlockCacheConfig {
        storage: StorageConfig {
            database: DatabaseConfig {
                path: temp_dir.path().to_path_buf(),
                sync_fetch_lookahead,
                sync_build_concurrency,
                ..Default::default()
            },
            ..Default::default()
        },
        db_version: 1,
        network: Network::Regtest(ActivationHeights::default()),
    };
    let db = ZainoDB::spawn(config, source).await.unwrap();
    (temp_dir, db)
}

/// Asserts two finalised DBs are equivalent across the load-bearing invariants: tip height,
/// the txout-set accumulator (txout-set correctness), and — per height — the stored block
/// hash (ordering / pairing / off-by-one) and cumulative chainwork (sequential threading).
async fn assert_finalised_dbs_equivalent(golden: &DbReader, candidate: &DbReader, top_height: u32) {
    assert_eq!(
        golden.db_height().await.unwrap(),
        candidate.db_height().await.unwrap(),
        "tip height differs"
    );
    assert_eq!(
        golden.get_tx_out_set_info_accumulator().await.unwrap(),
        candidate.get_tx_out_set_info_accumulator().await.unwrap(),
        "txout-set accumulator differs"
    );
    for height_raw in 0..=top_height {
        let height = Height(height_raw);
        let golden_block = golden
            .get_chain_block_by_height(height)
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("golden DB missing block at height {height_raw}"));
        let candidate_block = candidate
            .get_chain_block_by_height(height)
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("candidate DB missing block at height {height_raw}"));
        assert_eq!(
            golden_block.context.index.hash, candidate_block.context.index.hash,
            "block hash differs at height {height_raw}"
        );
        assert_eq!(
            golden_block.context.chainwork, candidate_block.context.chainwork,
            "cumulative chainwork differs at height {height_raw}"
        );
    }
}

// ***** Tier-1 #2: out-of-order completion *****

/// Reverse per-height fetch latency makes later heights complete before earlier ones, so the
/// pipeline's ordered buffer must reorder them before build. The resulting DB must be
/// equivalent to an in-order ("golden") sync of the same chain — proving out-of-order fetch
/// never leaks into out-of-order or mis-paired build.
///
/// `multi_thread` required: the concurrent fetch futures and the injected `sleep`s must make
/// real progress alongside the consumer.
#[tokio::test(flavor = "multi_thread")]
async fn pipeline_out_of_order_fetch_builds_in_order_db() {
    let blocks = load_test_vectors().unwrap().blocks;
    assert!(
        blocks.len() as u32 > TIP,
        "need more than {TIP} vector blocks for this test"
    );

    // Golden: no injected latency.
    let golden_source = FaultInjectingSource::no_faults(build_mockchain_source(blocks.clone()));
    let (_golden_dir, golden_db) = spawn_finalised_db(golden_source.clone()).await;
    golden_db
        .sync_to_height(Height(TIP), &golden_source)
        .await
        .unwrap();
    golden_db.wait_until_ready().await;

    // Out-of-order: later heights' fetches complete first.
    let ooo_source = FaultInjectingSource::with_reverse_delay(
        build_mockchain_source(blocks.clone()),
        TIP,
        Duration::from_millis(2),
    );
    let (_ooo_dir, ooo_db) = spawn_finalised_db(ooo_source.clone()).await;
    ooo_db
        .sync_to_height(Height(TIP), &ooo_source)
        .await
        .unwrap();
    ooo_db.wait_until_ready().await;

    let golden_reader = std::sync::Arc::new(golden_db).to_reader();
    let ooo_reader = std::sync::Arc::new(ooo_db).to_reader();
    assert_finalised_dbs_equivalent(&golden_reader, &ooo_reader, TIP).await;
}

// ***** Tier-3 #6: error propagation, no hang *****

/// The maximum a `sync_to_height` call may run before we treat it as hung. The chain is tiny,
/// so any real error surfaces in milliseconds; this only catches a deadlocked pipeline (e.g.
/// a producer task that outlives the failed consumer).
const NO_HANG_TIMEOUT: Duration = Duration::from_secs(30);
const FAIL_AT: u32 = 30;

/// A `get_block` error mid-chain must surface as `Err`, must not deadlock, and must not
/// commit at or past the failing height.
#[tokio::test(flavor = "multi_thread")]
async fn pipeline_getblock_error_propagates_without_hang() {
    let blocks = load_test_vectors().unwrap().blocks;
    let source =
        FaultInjectingSource::fail_block_at(build_mockchain_source(blocks.clone()), FAIL_AT);
    let (_dir, db) = spawn_finalised_db(source.clone()).await;

    let outcome =
        tokio::time::timeout(NO_HANG_TIMEOUT, db.sync_to_height(Height(TIP), &source)).await;
    let result = outcome.expect("sync_to_height hung on an injected getblock error");
    assert!(
        result.is_err(),
        "sync must surface the injected getblock error at height {FAIL_AT}"
    );

    db.wait_until_ready().await;
    let committed = db.db_height().await.unwrap();
    assert!(
        committed.is_none_or(|h| h.0 < FAIL_AT),
        "committed tip {committed:?} must stay below the failing height {FAIL_AT}"
    );
}

/// A `get_commitment_tree_roots` (treestate) error mid-chain must surface as `Err`, must not
/// deadlock, and must not commit at or past the failing height. The target hash is derived
/// exactly as the sync loop derives it (`BlockHash::from(block.hash().0)`).
#[tokio::test(flavor = "multi_thread")]
async fn pipeline_treestate_error_propagates_without_hang() {
    let blocks = load_test_vectors().unwrap().blocks;
    let fail_hash = BlockHash::from(
        blocks
            .iter()
            .find(|block| block.height == FAIL_AT)
            .expect("vector block at FAIL_AT")
            .zebra_block
            .hash()
            .0,
    );
    let source = FaultInjectingSource::fail_treestate_for_hash(
        build_mockchain_source(blocks.clone()),
        fail_hash,
    );
    let (_dir, db) = spawn_finalised_db(source.clone()).await;

    let outcome =
        tokio::time::timeout(NO_HANG_TIMEOUT, db.sync_to_height(Height(TIP), &source)).await;
    let result = outcome.expect("sync_to_height hung on an injected treestate error");
    assert!(
        result.is_err(),
        "sync must surface the injected treestate error at height {FAIL_AT}"
    );

    db.wait_until_ready().await;
    let committed = db.db_height().await.unwrap();
    assert!(
        committed.is_none_or(|h| h.0 < FAIL_AT),
        "committed tip {committed:?} must stay below the failing height {FAIL_AT}"
    );
}

// ***** Configurable lookahead *****

/// `storage.database.sync_fetch_lookahead` must actually bound fetch concurrency: with a small
/// configured depth and a uniform per-block fetch delay (so fetches pile up), the peak number
/// of concurrent in-flight `get_block` calls must never exceed the configured depth — and must
/// exceed one, proving the pipeline really runs fetches concurrently rather than serially.
#[tokio::test(flavor = "multi_thread")]
async fn pipeline_respects_configured_lookahead_bound() {
    const LOOKAHEAD: usize = 3;

    let blocks = load_test_vectors().unwrap().blocks;
    let source = FaultInjectingSource::with_uniform_delay(
        build_mockchain_source(blocks.clone()),
        Duration::from_millis(20),
    );
    let (_dir, db) = spawn_finalised_db_with_lookahead(source.clone(), LOOKAHEAD).await;

    db.sync_to_height(Height(TIP), &source).await.unwrap();
    db.wait_until_ready().await;

    let max_in_flight = source.max_in_flight();
    assert!(
        max_in_flight <= LOOKAHEAD,
        "peak in-flight fetches {max_in_flight} exceeded the configured lookahead {LOOKAHEAD}"
    );
    assert!(
        max_in_flight > 1,
        "fetches never overlapped (peak in-flight {max_in_flight}); the pipeline ran serially"
    );
}

/// A configured depth of 1 must degrade to a correct serial sync: no fetch ever overlaps
/// another, and the resulting DB is byte-equivalent to a default-depth (pipelined) sync.
#[tokio::test(flavor = "multi_thread")]
async fn lookahead_of_one_is_serial_and_correct() {
    let blocks = load_test_vectors().unwrap().blocks;
    assert!(
        blocks.len() as u32 > TIP,
        "need more than {TIP} vector blocks for this test"
    );

    // Reference: default (pipelined) depth.
    let reference_source = FaultInjectingSource::no_faults(build_mockchain_source(blocks.clone()));
    let (_ref_dir, reference_db) = spawn_finalised_db(reference_source.clone()).await;
    reference_db
        .sync_to_height(Height(TIP), &reference_source)
        .await
        .unwrap();
    reference_db.wait_until_ready().await;

    // Depth 1: with a uniform fetch delay, a serial pipeline can never hold two fetches at once.
    let serial_source = FaultInjectingSource::with_uniform_delay(
        build_mockchain_source(blocks.clone()),
        Duration::from_millis(5),
    );
    let (_serial_dir, serial_db) =
        spawn_finalised_db_with_lookahead(serial_source.clone(), 1).await;
    serial_db
        .sync_to_height(Height(TIP), &serial_source)
        .await
        .unwrap();
    serial_db.wait_until_ready().await;

    assert_eq!(
        serial_source.max_in_flight(),
        1,
        "depth 1 must never run two fetches concurrently"
    );

    let reference_reader = std::sync::Arc::new(reference_db).to_reader();
    let serial_reader = std::sync::Arc::new(serial_db).to_reader();
    assert_finalised_dbs_equivalent(&reference_reader, &serial_reader, TIP).await;
}

// ***** Parallel build: correctness + throughput *****

/// Parallel build must reproduce the serial chainwork prefix sum exactly. Build the same chain at
/// `sync_build_concurrency = 1` and at a wide concurrency *with reverse fetch latency* (so builds
/// also complete out of order), and assert the DBs are equivalent per height — proving the
/// ZERO-parent build plus the consumer's prefix-sum fixup reconstruct sequential chainwork
/// regardless of build-completion order.
///
/// `multi_thread` required: concurrent build tasks on the blocking pool must make real progress
/// alongside the consumer.
#[tokio::test(flavor = "multi_thread")]
async fn parallel_build_reproduces_serial_chainwork() {
    let blocks = load_test_vectors().unwrap().blocks;
    assert!(
        blocks.len() as u32 > TIP,
        "need more than {TIP} vector blocks for this test"
    );

    // Serial build (width 1), in order.
    let serial_source = FaultInjectingSource::no_faults(build_mockchain_source(blocks.clone()));
    let (_serial_dir, serial_db) =
        spawn_finalised_db_with_widths(serial_source.clone(), 8, 1).await;
    serial_db
        .sync_to_height(Height(TIP), &serial_source)
        .await
        .unwrap();
    serial_db.wait_until_ready().await;

    // Wide parallel build, with reverse fetch latency so builds finish out of order.
    let parallel_source = FaultInjectingSource::with_reverse_delay(
        build_mockchain_source(blocks.clone()),
        TIP,
        Duration::from_millis(2),
    );
    let (_parallel_dir, parallel_db) =
        spawn_finalised_db_with_widths(parallel_source.clone(), 8, 8).await;
    parallel_db
        .sync_to_height(Height(TIP), &parallel_source)
        .await
        .unwrap();
    parallel_db.wait_until_ready().await;

    let serial_reader = std::sync::Arc::new(serial_db).to_reader();
    let parallel_reader = std::sync::Arc::new(parallel_db).to_reader();
    assert_finalised_dbs_equivalent(&serial_reader, &parallel_reader, TIP).await;
}

/// With an injected per-block build cost making the pipeline deterministically build-bound,
/// raising `sync_build_concurrency` must cut wall-clock: builds run as busy-spins on the blocking
/// pool, so N concurrent builds finish faster (up to core count). This is the throughput proof —
/// that parallel build *increases* throughput, not merely runs correctly.
///
/// Skipped on a single core (nothing to parallelize). `multi_thread` required so the blocking
/// pool's spins run on separate OS threads concurrently.
#[tokio::test(flavor = "multi_thread")]
async fn parallel_build_increases_throughput() {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    if cores < 2 {
        eprintln!("skipping parallel_build_increases_throughput: needs >= 2 cores, have {cores}");
        return;
    }

    let blocks = load_test_vectors().unwrap().blocks;
    assert!(
        blocks.len() as u32 > TIP,
        "need more than {TIP} vector blocks for this test"
    );

    // Make each build ~2 ms of busy-spin CPU so build dominates the trivial mock fetch/write.
    // The guard clears the injected cost on drop — panic-safe, so it can't leak into sibling tests.
    let _build_cost = crate::chain_index::finalised_state::TestBuildCostGuard::set(2_000_000);

    let serial_time = {
        let source = FaultInjectingSource::no_faults(build_mockchain_source(blocks.clone()));
        let (_dir, db) = spawn_finalised_db_with_widths(source.clone(), 8, 1).await;
        let start = std::time::Instant::now();
        db.sync_to_height(Height(TIP), &source).await.unwrap();
        db.wait_until_ready().await;
        start.elapsed()
    };
    let parallel_time = {
        let source = FaultInjectingSource::no_faults(build_mockchain_source(blocks.clone()));
        let (_dir, db) = spawn_finalised_db_with_widths(source.clone(), 8, 4).await;
        let start = std::time::Instant::now();
        db.sync_to_height(Height(TIP), &source).await.unwrap();
        db.wait_until_ready().await;
        start.elapsed()
    };

    // Conservative: 4-way build on >= 2 cores should beat serial by a clear margin even with
    // dispatch overhead and CI noise (ideal is ~min(4, cores)x).
    assert!(
        parallel_time < serial_time.mul_f64(0.75),
        "parallel build (width 4: {parallel_time:?}) not meaningfully faster than serial \
         (width 1: {serial_time:?}); expected a build-bound speedup from sync_build_concurrency"
    );
}
