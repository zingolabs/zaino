//! Concurrency tests for the look-ahead fetch pipeline in
//! [`ZainoDB::sync_to_height`](crate::chain_index::finalised_state::ZainoDB::sync_to_height).
//!
//! The pipeline fetches up to `SYNC_FETCH_LOOKAHEAD` blocks concurrently and out of order,
//! but build+write must stay strictly height-ordered (`parent_chainwork` threads
//! sequentially and the writer demands height-contiguity). Every test here targets one
//! question: does out-of-order *fetching* ever leak into out-of-order — or mis-paired —
//! *building*?
//!
//! The enabling piece is [`FaultInjectingSource`], a thin `BlockchainSource` decorator that
//! wraps the existing mockchain and injects per-height fetch latency or targeted errors. It
//! touches no production code.
//!
//! Covered here (Tier-1 #2 and Tier-3 #6 of the pipeline test plan):
//! - [`pipeline_out_of_order_fetch_builds_in_order_db`] — reverse per-height latency makes
//!   later heights' fetches complete first; the resulting DB must still be byte-equivalent
//!   to an in-order (golden) sync.
//! - [`pipeline_getblock_error_propagates_without_hang`] /
//!   [`pipeline_treestate_error_propagates_without_hang`] — an injected error on either
//!   fetch leg must surface as `Err`, must not deadlock, and must not commit past the
//!   failing height.

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

/// Wraps a `BlockchainSource` and injects latency / errors on the two fetch legs the sync
/// pipeline uses (`get_block`, `get_commitment_tree_roots`); every other method delegates.
#[derive(Clone)]
struct FaultInjectingSource<S: BlockchainSource> {
    inner: S,
    faults: Arc<Faults>,
}

impl<S: BlockchainSource> FaultInjectingSource<S> {
    fn new(inner: S, faults: Faults) -> Self {
        Self {
            inner,
            faults: Arc::new(faults),
        }
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

/// Spawns a fresh v1 finalised DB over `source`. (`spawn` only touches the source on the
/// migration path, so injected faults fire only during the explicit `sync_to_height` call.)
async fn spawn_finalised_db<S: BlockchainSource>(source: S) -> (TempDir, ZainoDB) {
    let temp_dir = tempfile::tempdir().unwrap();
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
