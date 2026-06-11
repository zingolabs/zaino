use std::{sync::Arc, time::Duration};

use futures::stream::FuturesUnordered;
use proptest::{
    prelude::{Arbitrary as _, BoxedStrategy, Just},
    strategy::Strategy,
};
use rand::seq::IndexedRandom;
use tokio_stream::StreamExt as _;
use tonic::async_trait;
use zaino_common::{network::ActivationHeights, DatabaseConfig, Network, StorageConfig};
use zaino_fetch::jsonrpsee::response::address_deltas::{
    GetAddressDeltasParams, GetAddressDeltasResponse,
};
use zebra_chain::{
    block::arbitrary::{self, LedgerStateOverride},
    fmt::SummaryDebug,
    serialization::ZcashSerialize,
    transaction::SerializedTransaction,
    LedgerState,
};
use zebra_rpc::{
    client::{GetAddressBalanceRequest, GetAddressTxIdsRequest},
    methods::{AddressBalance, GetAddressUtxos},
};
use zebra_state::{FromDisk, HashOrHeight, IntoDisk as _};

use crate::{
    chain_index::{
        finalization_ceiling,
        non_finalised_state::ChainIndexSnapshot,
        source::{BlockchainSourceResult, GetTransactionLocation},
        tests::{init_tracing, poll::poll_until, proptest_blockgen::proptest_helpers::add_segment},
        types::BestChainLocation,
        NonFinalizedSnapshot,
    },
    BlockCacheConfig, BlockHash, BlockchainSource, ChainIndex, NodeBackedChainIndex,
    NodeBackedChainIndexSubscriber, TransactionHash,
};

/// The finalization ceiling for a snapshot's own best tip: the highest height
/// it serves from its own data, and the boundary below which the validator may
/// be consulted by passthrough.
fn snapshot_finalization_ceiling(snapshot: &ChainIndexSnapshot) -> crate::Height {
    finalization_ceiling(snapshot.get_nfs_snapshot().best_tip.height.0)
}

/// Handle all the boilerplate for a passthrough
fn passthrough_test(
    // The actual assertions. Takes as args:
    test: impl AsyncFn(
        // The mockchain, to use a a source of truth
        &ProptestMockchain,
        // The subscriber to test against
        NodeBackedChainIndexSubscriber<ProptestMockchain>,
        // A snapshot, which will have only the genesis block
        &ChainIndexSnapshot,
    ),
) {
    init_tracing();
    let network = Network::Regtest(ActivationHeights::default());
    // Long enough to have some finalized blocks to play with
    let segment_length = 120;
    // No need to worry about non-best chains for this test
    let branch_count = 1;

    // from this line to `runtime.block_on(async {` are all
    // copy-pasted. Could a macro get rid of some of this boilerplate?
    proptest::proptest!(proptest::test_runner::Config::with_cases(1), |(segments in make_branching_chain(branch_count, segment_length, network))| {
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_time().build().unwrap();
        runtime.block_on(async {
            let (genesis_segment, branching_segments) = segments;
            let mockchain = ProptestMockchain {
                genesis_segment,
                branching_segments,
                // Hold the finalized DB at genesis so the snapshot stays
                // Provisional: the always-leading NFS reaches the tip while the
                // finalized DB lags, so every block below the finalization
                // ceiling is served through the validator-passthrough gap. No
                // artificial per-call delay — the gap is deterministic.
                finalized_sync_cap: Arc::new(std::sync::atomic::AtomicU32::new(0)),
                best_branch_cache: Arc::new(std::sync::OnceLock::new()),
                tx_index: Arc::new(std::sync::OnceLock::new()),
                commitment_roots_cache: Arc::new(std::sync::OnceLock::new()),
            };
            let temp_dir: tempfile::TempDir = tempfile::tempdir().unwrap();
            let db_path: std::path::PathBuf = temp_dir.path().to_path_buf();

            let config = BlockCacheConfig {
                storage: StorageConfig {
                    database: DatabaseConfig {
                        path: db_path,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                db_version: 1,
                network,

            };

            let indexer = NodeBackedChainIndex::new(mockchain.clone(), config)
                .await
                .unwrap();
            let index_reader = indexer.subscriber();
            // 101 instead of 100 as heights are 0-indexed
            let expected_finalization_ceiling = (2 * segment_length) - 101;
            // Poll rather than sleeping: the always-leading NFS reaches the tip
            // (and so the expected finalization ceiling) as soon as the sync
            // task has walked the non-finalized window from the source. With no
            // artificial per-call delay this is fast; the budget only guards
            // against parallel-suite scheduler pressure.
            poll_until(
                "indexer to reach the expected finalization ceiling",
                Duration::from_secs(30),
                Duration::from_millis(50),
                || async {
                    let snapshot = index_reader.snapshot_nonfinalized_state().await.ok()?;
                    (snapshot_finalization_ceiling(&snapshot).0 as usize
                        == expected_finalization_ceiling)
                        .then_some(())
                },
            )
            .await;
            let snapshot = index_reader.snapshot_nonfinalized_state().await.unwrap();
            assert_eq!(
                snapshot_finalization_ceiling(&snapshot).0 as usize,
                expected_finalization_ceiling
            );
            assert!(!snapshot.is_resolved());

            test(&mockchain, index_reader, &snapshot).await;




        });
    })
}

#[test]
fn passthrough_find_fork_point() {
    // TODO: passthrough_test handles a good chunck of boilerplate, but there's
    // still a lot more inside of the closures being passed to passthrough_test.
    // Can we DRY out more of it?
    passthrough_test(async |mockchain, index_reader, snapshot| {
        // We use a futures-unordered instead of only a for loop
        // as this lets us call all the get_raw_transaction requests
        // at the same time and wait for them in parallel
        //
        // This lets the per-block passthrough source calls run concurrently.
        let mut parallel = FuturesUnordered::new();
        // As we only have one branch, arbitrary branch order is fine
        for (height, hash) in mockchain
            .all_blocks_arb_branch_order()
            .map(|block| (block.coinbase_height().unwrap(), block.hash()))
        {
            let index_reader = index_reader.clone();
            let snapshot = snapshot.clone();
            parallel.push(async move {
                let fork_point = index_reader
                    .find_fork_point(&snapshot, &hash.into())
                    .await
                    .unwrap();

                // Single branch: every block is on the best chain, so its fork
                // point is itself — served from the NFS window, or (below the
                // ceiling) via the validator-passthrough gap. Never None.
                assert_eq!(hash, fork_point.unwrap().0);
                assert_eq!(height, fork_point.unwrap().1);
            })
        }
        while let Some(_success) = parallel.next().await {}
    });
}

#[test]
fn passthrough_get_transaction_status() {
    passthrough_test(async |mockchain, index_reader, snapshot| {
        // We use a futures-unordered instead of only a for loop
        // as this lets us call all the get_raw_transaction requests
        // at the same time and wait for them in parallel
        //
        // This lets the per-block passthrough source calls run concurrently.
        let mut parallel = FuturesUnordered::new();
        // As we only have one branch, arbitrary branch order is fine
        for (height, txid) in mockchain.all_blocks_arb_branch_order().flat_map(|block| {
            block
                .transactions
                .iter()
                .map(|transaction| (block.coinbase_height().unwrap(), transaction.hash()))
                .collect::<Vec<_>>()
        }) {
            let index_reader = index_reader.clone();
            let snapshot = snapshot.clone();
            parallel.push(async move {
                let transaction_status = index_reader
                    .get_transaction_status(&snapshot, &txid.into())
                    .await
                    .unwrap();

                // Single branch: every transaction is on the best chain, served
                // from the NFS window or (below the ceiling) the passthrough gap.
                let Some(BestChainLocation::Block(_block_hash, transaction_height)) =
                    transaction_status.0
                else {
                    panic!("expected best chain location")
                };
                assert_eq!(height, transaction_height);
                assert!(transaction_status.1.is_empty());
            })
        }
        while let Some(_success) = parallel.next().await {}
    });
}

#[test]
fn passthrough_get_raw_transaction() {
    passthrough_test(async |mockchain, index_reader, snapshot| {
        // We use a futures-unordered instead of only a for loop
        // as this lets us call all the get_raw_transaction requests
        // at the same time and wait for them in parallel
        //
        // This lets the per-block passthrough source calls run concurrently.
        let mut parallel = FuturesUnordered::new();
        // As we only have one branch, arbitrary branch order is fine
        for (expected_transaction, height) in
            mockchain.all_blocks_arb_branch_order().flat_map(|block| {
                block
                    .transactions
                    .iter()
                    .map(|transaction| (transaction, block.coinbase_height().unwrap()))
                    .collect::<Vec<_>>()
            })
        {
            let index_reader = index_reader.clone();
            let snapshot = snapshot.clone();
            parallel.push(async move {
                let actual_transaction = index_reader
                    .get_raw_transaction(
                        &snapshot,
                        &TransactionHash::from(expected_transaction.hash()),
                    )
                    .await
                    .unwrap();
                let Some((raw_transaction, _branch_id)) = actual_transaction else {
                    panic!("missing transaction at height {}", height.0)
                };
                assert_eq!(
                    raw_transaction,
                    SerializedTransaction::from(expected_transaction.clone()).as_ref()
                )
            })
        }
        while let Some(_success) = parallel.next().await {}
    });
}

#[test]
fn passthrough_best_chaintip() {
    passthrough_test(async |mockchain, index_reader, snapshot| {
        let tip = index_reader.best_chaintip(snapshot).await.unwrap();
        // best_chaintip derives from the always-leading NFS, which sits at the
        // full chain tip even while the finalized DB lags behind.
        assert_eq!(
            tip.height.0,
            mockchain
                .best_branch()
                .last()
                .unwrap()
                .coinbase_height()
                .unwrap()
                .0
        );
    })
}

#[test]
fn passthrough_get_block_height() {
    passthrough_test(async |mockchain, index_reader, snapshot| {
        // We use a futures-unordered instead of only a for loop
        // as this lets us call all the get_raw_transaction requests
        // at the same time and wait for them in parallel
        //
        // This lets the per-block passthrough source calls run concurrently.
        let mut parallel = FuturesUnordered::new();

        for (expected_height, hash) in mockchain
            .all_blocks_arb_branch_order()
            .map(|block| (block.coinbase_height().unwrap(), block.hash()))
        {
            let index_reader = index_reader.clone();
            let snapshot = snapshot.clone();
            parallel.push(async move {
                let height = index_reader
                    .get_block_height(&snapshot, hash.into())
                    .await
                    .unwrap();
                // Every block is served: the NFS window, or (below the ceiling)
                // the validator-passthrough gap.
                assert_eq!(height, Some(expected_height.into()));
            });
        }
        while let Some(_success) = parallel.next().await {}
    })
}

#[test]
fn passthrough_get_block_range() {
    passthrough_test(async |mockchain, index_reader, snapshot| {
        // We use a futures-unordered instead of only a for loop
        // as this lets us call all the get_raw_transaction requests
        // at the same time and wait for them in parallel
        //
        // This lets the per-block passthrough source calls run concurrently.
        let mut parallel = FuturesUnordered::new();

        for expected_start_height in mockchain
            .all_blocks_arb_branch_order()
            .map(|block| block.coinbase_height().unwrap())
        {
            let expected_end_height = (expected_start_height + 9).unwrap();
            if expected_end_height.0 as usize <= mockchain.all_blocks_arb_branch_order().count() {
                let index_reader = index_reader.clone();
                let snapshot = snapshot.clone();
                parallel.push(async move {
                    let block_range_stream = index_reader.get_block_range(
                        &snapshot,
                        expected_start_height.into(),
                        Some(expected_end_height.into()),
                    );
                    // Every height up to the tip is served (NFS window ∪
                    // passthrough gap), so the range is always servable.
                    let mut block_range_stream = Box::pin(block_range_stream.unwrap());
                    let mut num_blocks_in_stream = 0;
                    while let Some(block) = block_range_stream.next().await {
                        let expected_block = mockchain
                            .all_blocks_arb_branch_order()
                            .nth(expected_start_height.0 as usize + num_blocks_in_stream)
                            .unwrap()
                            .zcash_serialize_to_vec()
                            .unwrap();
                        assert_eq!(block.unwrap(), expected_block);
                        num_blocks_in_stream += 1;
                    }
                    assert_eq!(
                        num_blocks_in_stream,
                        // 10 blocks, unless the range runs past the best tip.
                        10.min(
                            snapshot
                                .get_nfs_snapshot()
                                .best_tip
                                .height
                                .0
                                .saturating_sub(expected_start_height.0)
                                + 1
                        ) as usize
                    );
                });
            }
        }
        while let Some(_success) = parallel.next().await {}
    })
}

#[test]
fn make_chain() {
    init_tracing();
    let network = Network::Regtest(ActivationHeights::default());
    let segment_length = 12;

    let branch_count = 2;

    // default is 256. As each case takes multiple seconds, this seems too many.
    // TODO: this should be higher than 1. Currently set to 1 for ease of iteration
    proptest::proptest!(proptest::test_runner::Config::with_cases(1), |(segments in make_branching_chain(branch_count, segment_length, network))| {
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_time().build().unwrap();
        runtime.block_on(async {
            let (genesis_segment, branching_segments) = segments;
            let mockchain = ProptestMockchain {
                genesis_segment,
                branching_segments,
                // No cap: the finalized DB syncs all the way to the ceiling, so
                // the snapshot resolves.
                finalized_sync_cap: Arc::new(std::sync::atomic::AtomicU32::new(u32::MAX)),
                best_branch_cache: Arc::new(std::sync::OnceLock::new()),
                tx_index: Arc::new(std::sync::OnceLock::new()),
                commitment_roots_cache: Arc::new(std::sync::OnceLock::new()),
            };
            let temp_dir: tempfile::TempDir = tempfile::tempdir().unwrap();
            let db_path: std::path::PathBuf = temp_dir.path().to_path_buf();

            let config = BlockCacheConfig {
                storage: StorageConfig {
                    database: DatabaseConfig {
                        path: db_path,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                db_version: 1,
                network,

            };

            let indexer = NodeBackedChainIndex::new(mockchain.clone(), config)
                .await
                .unwrap();
            let index_reader = indexer.subscriber();
            let expected_block_count = segment_length * (branch_count + 1);
            let snapshot = poll_until(
                "indexer to ingest the full proptest chain",
                Duration::from_secs(10),
                Duration::from_millis(25),
                || async {
                    let snapshot = index_reader.snapshot_nonfinalized_state().await.ok()?;
                    (snapshot.resolved_nfs_snapshot()?.blocks.len() == expected_block_count)
                        .then_some(snapshot)
                },
            )
            .await;
            let non_finalized_snapshot = snapshot.resolved_nfs_snapshot().expect("not synced");
            let best_tip_hash = non_finalized_snapshot.best_tip.hash;
            let best_tip_block = non_finalized_snapshot
                .get_chainblock_by_hash(&best_tip_hash)
                .unwrap();
            for (hash, block) in &non_finalized_snapshot.blocks {
                if hash != &best_tip_hash {
                    assert!(
                        block.provisional_cumulative_work(non_finalized_snapshot)
                            <= best_tip_block.provisional_cumulative_work(non_finalized_snapshot)
                    );
                    if non_finalized_snapshot.heights_to_hashes.get(&block.height()) == Some(block.hash()) {
                        assert_eq!(index_reader.find_fork_point(&snapshot, hash).await.unwrap().unwrap().0, *hash);
                    } else {
                        assert_ne!(index_reader.find_fork_point(&snapshot, hash).await.unwrap().unwrap().0, *hash);
                    }
                }
            }
            assert_eq!(non_finalized_snapshot.heights_to_hashes.len(), (segment_length * 2) );
            assert_eq!(
                non_finalized_snapshot.blocks.len(),
                segment_length * (branch_count + 1)
            );
        });
    });
}

/// Sapling and Orchard commitment-tree `(root, tree_size)` for a block, as
/// returned by [`BlockchainSource::get_commitment_tree_roots`].
type CommitmentRoots = (
    Option<(zebra_chain::sapling::tree::Root, u64)>,
    Option<(zebra_chain::orchard::tree::Root, u64)>,
);

type SaplingFrontier = incrementalmerkletree::frontier::Frontier<sapling_crypto::Node, 32>;
type OrchardFrontier =
    incrementalmerkletree::frontier::Frontier<zebra_chain::orchard::tree::Node, 32>;

/// Append each block's note commitments to the running frontiers, recording the
/// resulting `(root, tree_size)` per block hash into `map`. Returns the final
/// frontiers so a branch can continue from where the genesis segment left off.
fn fold_commitment_roots<'a>(
    blocks: impl Iterator<Item = &'a Arc<zebra_chain::block::Block>>,
    mut sapling: Option<SaplingFrontier>,
    mut orchard: Option<OrchardFrontier>,
    map: &mut std::collections::HashMap<BlockHash, CommitmentRoots>,
) -> (Option<SaplingFrontier>, Option<OrchardFrontier>) {
    for block in blocks {
        for transaction in &block.transactions {
            for sap in transaction.sapling_note_commitments() {
                let node = sapling_crypto::Node::from_bytes(sap.to_bytes()).unwrap();
                sapling
                    .get_or_insert_with(SaplingFrontier::empty)
                    .append(node);
            }
            for orc in transaction.orchard_note_commitments() {
                let node = zebra_chain::orchard::tree::Node::from(*orc);
                orchard
                    .get_or_insert_with(OrchardFrontier::empty)
                    .append(node);
            }
        }
        map.insert(
            BlockHash::from(block.hash()),
            (
                sapling.as_ref().map(|f| {
                    (
                        zebra_chain::sapling::tree::Root::from_bytes(f.root().to_bytes()),
                        f.tree_size(),
                    )
                }),
                orchard.as_ref().map(|f| {
                    (
                        zebra_chain::orchard::tree::Root::from_bytes(f.root().as_bytes()),
                        f.tree_size(),
                    )
                }),
            ),
        );
    }
    (sapling, orchard)
}

#[derive(Clone)]
struct ProptestMockchain {
    genesis_segment: ChainSegment,
    branching_segments: Vec<ChainSegment>,
    /// Caps the height the finalized DB may sync to (`u32::MAX` = no cap),
    /// surfaced via [`BlockchainSource::finalized_sync_cap`]. Holding it below
    /// the finalization ceiling keeps the snapshot deterministically Provisional
    /// — the always-leading NFS reaches the tip while the finalized DB stays
    /// behind — so the catch-up gap (served by passthrough) is exercised without
    /// any artificial per-call delay.
    finalized_sync_cap: Arc<std::sync::atomic::AtomicU32>,
    /// Cached result of `best_branch()`. The best branch is pure function of
    /// the other fields (which are never mutated after construction), so it's
    /// safe to memoize. Shared via `Arc` so `mockchain.clone()` — which
    /// happens per-future in the test bodies via `index_reader.clone()` —
    /// reuses the same cache rather than recomputing per clone.
    best_branch_cache: Arc<std::sync::OnceLock<SummaryDebug<Vec<Arc<zebra_chain::block::Block>>>>>,
    /// Cached txid → (tx, location) index. Built lazily on first `get_transaction`
    /// call. Replaces the O(N_blocks × M_txs) linear scan that recomputed
    /// `transaction.hash()` on every iteration — the dominant cost in the
    /// tx-iterating passthrough tests.
    tx_index: Arc<
        std::sync::OnceLock<
            std::collections::HashMap<
                zebra_chain::transaction::Hash,
                (
                    Arc<zebra_chain::transaction::Transaction>,
                    GetTransactionLocation,
                ),
            >,
        >,
    >,
    /// Cached commitment-tree roots keyed by block hash, built lazily in one
    /// incremental pass. The finalized DB requires real roots (it rejects
    /// `None`), and recomputing them by folding from genesis on every call was
    /// O(N) crypto per call → O(N²) across a sync; this makes lookups O(1).
    commitment_roots_cache:
        Arc<std::sync::OnceLock<std::collections::HashMap<BlockHash, CommitmentRoots>>>,
}

impl ProptestMockchain {
    fn best_branch(&self) -> &SummaryDebug<Vec<Arc<zebra_chain::block::Block>>> {
        self.best_branch_cache.get_or_init(|| {
            let mut best_branch_and_work = None;
            for branch in self.branching_segments.clone() {
                let branch_chainwork: u128 = branch
                    .iter()
                    .map(|block| {
                        block
                            .header
                            .difficulty_threshold
                            .to_work()
                            .unwrap()
                            .as_u128()
                    })
                    .sum();
                match best_branch_and_work {
                    Some((ref _b, w)) => {
                        if w < branch_chainwork {
                            best_branch_and_work = Some((branch, branch_chainwork))
                        }
                    }
                    None => best_branch_and_work = Some((branch, branch_chainwork)),
                }
            }
            let mut combined = self.genesis_segment.clone();
            combined.append(&mut best_branch_and_work.unwrap().0.clone());
            combined
        })
    }

    /// Builds (lazily) and returns the tx-by-hash index.
    fn tx_index(
        &self,
    ) -> &std::collections::HashMap<
        zebra_chain::transaction::Hash,
        (
            Arc<zebra_chain::transaction::Transaction>,
            GetTransactionLocation,
        ),
    > {
        self.tx_index.get_or_init(|| {
            let best = self.best_branch().clone();
            let mut map = std::collections::HashMap::new();
            for block in self.all_blocks_arb_branch_order() {
                let location = if best.contains(block) {
                    GetTransactionLocation::BestChain(block.coinbase_height().unwrap())
                } else {
                    GetTransactionLocation::NonbestChain
                };
                for tx in block.transactions.iter() {
                    map.insert(tx.hash(), (tx.clone(), location.clone()));
                }
            }
            map
        })
    }

    fn all_blocks_arb_branch_order(&self) -> impl Iterator<Item = &Arc<zebra_chain::block::Block>> {
        self.genesis_segment.iter().chain(
            self.branching_segments
                .iter()
                .flat_map(|branch| branch.iter()),
        )
    }
}

#[async_trait]
impl BlockchainSource for ProptestMockchain {
    /// Returns the block by hash or height
    async fn get_block(
        &self,
        id: HashOrHeight,
    ) -> BlockchainSourceResult<Option<Arc<zebra_chain::block::Block>>> {
        match id {
            HashOrHeight::Hash(hash) => {
                let matches_hash = |block: &&Arc<zebra_chain::block::Block>| block.hash() == hash;
                Ok(self
                    .genesis_segment
                    .iter()
                    .find(matches_hash)
                    .or_else(|| {
                        self.branching_segments
                            .iter()
                            .flat_map(|vec| vec.iter())
                            .find(matches_hash)
                    })
                    .cloned())
            }
            // This implementation selects a block from a random branch instead
            // of the best branch. This is intended to simulate reorgs
            HashOrHeight::Height(height) => Ok(self
                .genesis_segment
                .iter()
                .find(|block| block.coinbase_height().unwrap() == height)
                .cloned()
                .or_else(|| {
                    self.branching_segments
                        .choose(&mut rand::rng())
                        .unwrap()
                        .iter()
                        .find(|block| block.coinbase_height().unwrap() == height)
                        .cloned()
                })),
        }
    }

    /// Returns the block commitment tree data by hash.
    ///
    /// The NFS sync calls this once per block as it walks the window, so roots
    /// are precomputed for every block in one incremental pass and cached by
    /// hash (O(1) lookups). The previous implementation folded the frontier
    /// from genesis on every call — O(N) cryptographic hashing per call, O(N²)
    /// across the window walk, which dominated these tests' runtime.
    async fn get_commitment_tree_roots(
        &self,
        id: BlockHash,
    ) -> BlockchainSourceResult<CommitmentRoots> {
        let roots = self.commitment_roots_cache.get_or_init(|| {
            let mut map = std::collections::HashMap::new();
            // Genesis segment, then each branch continuing from the genesis-end
            // frontier — one incremental fold over the whole tree.
            let (sapling, orchard) =
                fold_commitment_roots(self.genesis_segment.iter(), None, None, &mut map);
            for branch in self.branching_segments.iter() {
                fold_commitment_roots(branch.iter(), sapling.clone(), orchard.clone(), &mut map);
            }
            map
        });
        Ok(roots.get(&id).cloned().unwrap_or((None, None)))
    }

    /// Returns the sapling and orchard treestate by hash
    async fn get_treestate(
        &self,
        _id: BlockHash,
    ) -> BlockchainSourceResult<(Option<Vec<u8>>, Option<Vec<u8>>)> {
        // I don't think this is used for sync?
        unimplemented!()
    }

    /// Returns the complete list of txids currently in the mempool.
    async fn get_mempool_txids(
        &self,
    ) -> BlockchainSourceResult<Option<Vec<zebra_chain::transaction::Hash>>> {
        Ok(Some(Vec::new()))
    }

    /// Returns the transaction by txid
    async fn get_transaction(
        &self,
        txid: TransactionHash,
    ) -> BlockchainSourceResult<
        Option<(
            Arc<zebra_chain::transaction::Transaction>,
            GetTransactionLocation,
        )>,
    > {
        Ok(self.tx_index().get(&txid.into()).cloned())
    }

    /// Returns the hash of the block at the tip of the best chain.
    async fn get_best_block_hash(
        &self,
    ) -> BlockchainSourceResult<Option<zebra_chain::block::Hash>> {
        Ok(Some(self.best_branch().last().unwrap().hash()))
    }

    /// Returns the hash of the block at the tip of the best chain.
    async fn get_best_block_height(
        &self,
    ) -> BlockchainSourceResult<Option<zebra_chain::block::Height>> {
        Ok(Some(
            self.best_branch()
                .last()
                .unwrap()
                .coinbase_height()
                .unwrap(),
        ))
    }

    fn finalized_sync_cap(&self) -> Option<crate::Height> {
        match self
            .finalized_sync_cap
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            u32::MAX => None,
            cap => Some(crate::Height(cap)),
        }
    }

    /// Get a listener for new nonfinalized blocks,
    /// if supported
    async fn nonfinalized_listener(
        &self,
    ) -> Result<
        Option<
            tokio::sync::mpsc::Receiver<(zebra_chain::block::Hash, Arc<zebra_chain::block::Block>)>,
        >,
        Box<dyn std::error::Error + Send + Sync>,
    > {
        let (sender, receiver) = tokio::sync::mpsc::channel(1_000);
        let self_clone = self.clone();
        tokio::task::spawn(async move {
            for block in self_clone.all_blocks_arb_branch_order() {
                sender.send((block.hash(), block.clone())).await.unwrap()
            }
            // don't drop the sender
            std::mem::forget(sender);
        })
        .await
        .unwrap();
        Ok(Some(receiver))
    }

    async fn get_subtree_roots(
        &self,
        _pool: crate::chain_index::ShieldedPool,
        _start_index: u16,
        _max_entries: Option<u16>,
    ) -> BlockchainSourceResult<Vec<([u8; 32], u32)>> {
        todo!()
    }

    // ********** Transparent address methods **********

    async fn get_address_deltas(
        &self,
        _params: GetAddressDeltasParams,
    ) -> BlockchainSourceResult<GetAddressDeltasResponse> {
        //
        todo!()
    }

    async fn get_address_balance(
        &self,
        _address_strings: GetAddressBalanceRequest,
    ) -> BlockchainSourceResult<AddressBalance> {
        //
        todo!()
    }

    async fn get_address_txids(
        &self,
        _request: GetAddressTxIdsRequest,
    ) -> BlockchainSourceResult<Vec<TransactionHash>> {
        //
        todo!()
    }

    async fn get_address_utxos(
        &self,
        _address_strings: GetAddressBalanceRequest,
    ) -> BlockchainSourceResult<Vec<GetAddressUtxos>> {
        //
        todo!()
    }
}

type ChainSegment = SummaryDebug<Vec<Arc<zebra_chain::block::Block>>>;

fn make_branching_chain(
    // The number of separate branches, after the branching point at the tip
    // of the initial segment.
    num_branches: usize,
    // The length of the initial segment, and of the branches
    // TODO: it would be useful to allow branches of different lengths.
    chain_size: usize,
    network_override: Network,
) -> BoxedStrategy<(ChainSegment, Vec<ChainSegment>)> {
    let network_override = Some(network_override.to_zebra_network());
    add_segment(
        SummaryDebug(Vec::new()),
        network_override.clone(),
        chain_size,
    )
    .prop_flat_map(move |segment| {
        (
            Just(segment.clone()),
            LedgerState::arbitrary_with(LedgerStateOverride {
                height_override: segment.last().unwrap().coinbase_height().unwrap() + 1,
                previous_block_hash_override: Some(segment.last().unwrap().hash()),
                network_upgrade_override: None,
                transaction_version_override: None,
                transaction_has_valid_network_upgrade: true,
                always_has_coinbase: true,
                network_override: network_override.clone(),
            }),
        )
    })
    .prop_flat_map(move |(segment, ledger)| {
        (
            Just(segment),
            std::iter::repeat_with(|| {
                zebra_chain::block::Block::partial_chain_strategy(
                    ledger.clone(),
                    chain_size,
                    arbitrary::allow_all_transparent_coinbase_spends,
                    true,
                )
            })
            .take(num_branches)
            .collect::<Vec<_>>(),
        )
    })
    .boxed()
}

mod proptest_helpers {

    use proptest::prelude::{Arbitrary, BoxedStrategy, Strategy};
    use zebra_chain::{
        block::{
            arbitrary::{allow_all_transparent_coinbase_spends, LedgerStateOverride},
            Block, Height,
        },
        parameters::{Network, GENESIS_PREVIOUS_BLOCK_HASH},
        LedgerState,
    };

    use super::ChainSegment;

    pub(super) fn add_segment(
        previous_chain: ChainSegment,
        network_override: Option<Network>,
        segment_length: usize,
    ) -> BoxedStrategy<ChainSegment> {
        LedgerState::arbitrary_with(LedgerStateOverride {
            height_override: Some(
                previous_chain
                    .last()
                    .map(|block| (block.coinbase_height().unwrap() + 1).unwrap())
                    .unwrap_or(Height(0)),
            ),
            previous_block_hash_override: Some(
                previous_chain
                    .last()
                    .map(|block| block.hash())
                    .unwrap_or(GENESIS_PREVIOUS_BLOCK_HASH),
            ),
            network_upgrade_override: None,
            transaction_version_override: None,
            transaction_has_valid_network_upgrade: true,
            always_has_coinbase: true,
            network_override,
        })
        .prop_flat_map(move |ledger| {
            Block::partial_chain_strategy(
                ledger,
                segment_length,
                allow_all_transparent_coinbase_spends,
                true,
            )
        })
        .prop_map(move |new_segment| {
            let mut full_chain = previous_chain.clone();
            full_chain.extend_from_slice(&new_segment);
            full_chain
        })
        .boxed()
    }
}
